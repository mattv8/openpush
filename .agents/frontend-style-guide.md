# OpenPush desktop frontend style guide

Use this guide when you add or change UI in the Tauri desktop app. It
describes the implemented design: a Pushbullet-style messaging layout drawn
in the visual language of Ragtime's **Modern** (VS Code workbench) theme.
`.agents/development.md` still governs security, data, and evidence rules;
this guide covers presentation only.

## 1. Sources of truth

Resolve conflicts in this order:

1. Behaviour and tests: `apps/desktop/src/App.tsx`, `App.test.tsx`,
   `packages/desktop-ui/src/index.tsx`, `index.test.tsx`.
2. Tokens: `packages/desktop-ui/src/op-tokens.css`.
3. Shared component CSS: `packages/desktop-ui/src/styles.css`.
4. App shell and view CSS: `apps/desktop/src/app.css`.
5. This guide.

| Path | Owns |
|---|---|
| `packages/desktop-ui/src/op-tokens.css` | `--op-*` tokens, Droid Sans `@font-face`, reduced-motion rule |
| `packages/desktop-ui/src/styles.css` | Titlebar and window controls, rail, conversation list, recipient picker, composer field, resize sash |
| `packages/desktop-ui/src/index.tsx` | `AppTitlebar`, `Panel` (rail), `ConversationList`, `RecipientPicker`, `Composer`, `detectPlatform`, re-exported Lucide icons |
| `packages/desktop-ui/src/ResizeHandle.tsx` | Pane sash (port of Ragtime's `ResizeHandle`) |
| `apps/desktop/src/App.tsx` | Shell layout, thread header, disclosures, message list, gateway selector, settings, onboarding, layout persistence |
| `apps/desktop/src/app.css` | Shell, thread pane, bubbles, settings, onboarding, buttons |
| `apps/desktop/src/bridge.ts` | DTO types and the browser fixture data |

Ragtime references, for intent only. Do not import from Ragtime:
`~/GitRepos/ragtime/ragtime/frontend/src/styles/themes/modern.css` and
`~/GitRepos/ragtime/.agents/ragtime-ux-style-guide.md`.

## 2. Design principles

- **Workbench, not web page.** The window is one bounded workstation of
  docked panes. Depth comes from tonal surface steps and 1px borders. Do not
  use drop shadows, floating cards, or decorative margins.
- **Pushbullet placement.** Navigation rail on the left, conversation list,
  then the conversation. The composer sits at the bottom of the conversation,
  incoming bubbles on the left, outgoing on the right. Icon-first controls.
- **Dense and calm.** Use a 4px grid, 28px controls, and small type. Use the
  accent color for one primary action per area and for the selection state.
- **Honest status.** Show every state as icon plus text, never color alone.
  Keep the encryption disclosures visible. Never imply carrier SMS/MMS is
  end-to-end encrypted, and never imply native OS notifications exist.
- **Native-feeling chrome.** Window controls follow the host platform (§5).

## 3. Tokens

Every themeable value must use a `--op-*` custom property. Hard-coded colors
are allowed only for: traffic-light colors, the Windows close-hover red
(`#c42b1c`), white text on accent fills, and `rgba(255,255,255,.75)` bubble
metadata on the outgoing accent bubble.

### 3.1 Surfaces and text

| Token | Dark | Light | Use |
|---|---|---|---|
| `--op-surface-desk` | `#252526` | `#f3f3f3` | Shell background |
| `--op-surface-chrome` | `#181818` | `#f8f8f8` | Titlebar, rail, thread header |
| `--op-surface-panel` | `#181818` | `#f8f8f8` | Thread list, disclosures row, composer area |
| `--op-surface-editor` | `#1f1f1f` | `#fff` | Conversation pane, message list |
| `--op-surface-widget` | `#202020` | `#fff` | Inputs, composer field, chips, dropdowns |
| `--op-surface-hover` | `#2a2d2e` | `#f3f3f3` | Hover on borderless buttons and rows |
| `--op-surface-active` | `#313131` | `#ececec` | Selected row, incoming bubble |
| `--op-border` | `#2b2b2b` | `#e5e5e5` | Structural 1px borders |
| `--op-border-strong` | `#3c3c3c` | `#cecece` | Secondary buttons, incoming bubble edge |
| `--op-text-strong` | `#fff` | `#1f1f1f` | Titles, headings |
| `--op-text-primary` | `#ccc` | `#3b3b3b` | Body text |
| `--op-text-secondary` | `#9d9d9d` | `#616161` | Descriptions, previews, icon buttons |
| `--op-text-muted` | `#8c8c8c` | `#616161` | Metadata, hints, inactive rail icons |
| `--op-accent` / `--op-accent-hover` | `#0078d4` / `#026ec1` | `#005fb8` / `#0258a8` | Primary action, selection, outgoing bubble, sash highlight |
| `--op-accent-text` | `#fff` | `#fff` | Text on accent |
| `--op-success` / `--op-error` | `#89d185` / `#f48771` | `#2e7d32` / `#c72e0f` | Connection dot, status icons |
| `--op-warning-text` / `--op-warning-border` | `#f2cc60` | `#7a5200` | Sending-unavailable reason, public-link warning |
| `--op-focus` | `#0078d4` | `#005fb8` | Focus rings |

### 3.2 Dimensions and type

| Token or value | Value |
|---|---|
| `--op-titlebar-height` | 35px |
| `--op-toolbar-height` | 32px |
| `--op-control-height` | 28px (buttons, inputs, selects, icon buttons) |
| `--op-control-radius` | 4px (controls, chips, file cards) |
| Composer field and bubble radius | 8px |
| Pill radius | 999px (connection chip, disclosure pills) |
| `--op-rail-width` | 48px |
| Spacing | 4px grid: 4 / 8 / 12 / 16 / 24 |
| `--op-font-body` | Droid Sans (bundled, Apache-2.0), then system UI fonts |
| `--op-font-mono` | System mono stack. Use it for URLs and server origins. |
| Type scale | 11px metadata and hints · 12px secondary and labels · 13px buttons and wordmark · 14px titles and h2 · 15px message and composer text |

### 3.3 Themes

- `#desktop-shell` and `#composer-shell` carry one class: `theme-system`,
  `theme-light`, or `theme-dark`. Do not apply theme values as inline styles.
- `theme-system` follows `prefers-color-scheme`. When you add a light-mode
  override, add it to **both** `.theme-light` and the system-light
  `@media (prefers-color-scheme: light) .theme-system` block. Light values
  are duplicated on purpose.
- `styles.css` defines legacy aliases (`--surface-0`, `--text`, …) that map
  to `--op-*`. Do not use the aliases in new code.
- The `prefers-reduced-motion` block in `op-tokens.css` disables
  transitions. Keep motion limited to short opacity transitions (≤ 0.12s).

## 4. Layout

```
#desktop-shell[data-platform][data-window-focused].theme-*
├─ #desktop-titlebar (35px, data-tauri-drag-region)
└─ #desktop-body
   ├─ #desktop-rail (48px, never resizes)
   ├─ #thread-list (hidden while Settings is open)
   ├─ ResizeHandle "Resize thread list" (not rendered while Settings is open)
   └─ #conversation-pane[data-view]
      ├─ #thread-pane-header (title, #connection-status, .header-actions)
      ├─ #security-disclosures
      └─ #settings-view | #onboarding-view | #message-list + ResizeHandle "Resize composer" + #composer-area
```

- **Views.** The rail switches between `conversations` and `settings`.
  Settings replaces the thread list and the conversation. It keeps the
  thread header with the `h1` "Settings", the connection chip, and the
  disclosure row. It hides the header actions.
- **Onboarding** appears in the conversation pane only when the host is not
  connected and no conversation is selected.
- **Thread list sash.** Width 200–480px (default 280). Dragging below 120px
  collapses the list, and dragging the collapsed strip restores it under the
  pointer. Enter toggles collapse; arrow keys move 8px (Shift: 32px);
  Home/End jump to min/max. Clicking the active Conversations rail button
  also toggles the list. That button carries `aria-expanded` and
  `aria-controls="thread-list"`.
- **Composer sash.** Composer height ranges from its natural minimum (one
  text line plus toolbar; taller when attachments are present) to 50% of the
  pane. Double-click resets it to auto-grow. The composer window has the
  same sash.
- **Persistence.** `localStorage["openpush.layout.v1"]` stores
  `{ listWidth, listCollapsed, composerHeight }`. Parse it defensively and
  clamp on load. Persist only after a user drag, key press, or double-click.
  Never persist a value clamped for the current window size; the main and
  composer windows share this key.
- **Composer window** (`?window=composer&conversationId=…`): titlebar,
  disclosures, message list, sash, and composer. No rail and no list.
- **Main pane.** The conversation pane never collapses. Only the list
  collapses.

## 5. Platform chrome

The app draws its own window controls in React. Do not switch to native
decorations: the composer window must close through `closeAfterSave` so the
draft is flushed first.

`detectPlatform()` returns `macos`, `windows`, or `linux`. Pass it to
`AppTitlebar` and set it as `data-platform` on the shell. `AppTitlebar`
renders only the current platform's control set, inside one
`#window-controls` toolbar.

| | macOS | Windows / Linux |
|---|---|---|
| Position | Left, 12px inset, before the wordmark | Right, after `.titlebar-spacer` |
| Controls | 12px circles, 8px gap: close `#ff5f57`, minimize `#febc2e`, zoom `#28c840` | 46px × full titlebar height: minimize, maximize, close (Lucide 10px) |
| Hover | Group hover reveals × − + glyphs | `--op-surface-hover`; close turns `#c42b1c` with white glyph |
| Inactive window | All dots `#4d4d4d` (light: `#d1d1d1`) via `[data-window-focused="false"]` | Glyphs use `--op-text-muted` |
| Composer window | Red close active; yellow and green `disabled`, `aria-hidden`, `tabIndex=-1`, grey | Close button only |

Accessible names are fixed: "Minimize window", "Maximize window",
"Close window", and "Close composer". Main-window close calls
`closeAfterSave(() => bridge.window("close"))`. The native host may hide the
window to the tray.

## 6. Components

### Rail (`Panel`)

- Product mark at the top, then Conversations (`MessageCircle`) and
  Settings (`Settings`), Lucide 20px. A connection dot sits at the bottom
  with `role="status"`.
- Buttons are 46×28 and transparent, using `--op-text-muted`. The active
  button has `aria-current="page"`, a 2px accent left border, accent icon,
  and a 14% accent tint.

### Thread list

- Toolbar (32px): recipient combobox ("Search or start new") and a
  New message icon button (`MessageSquarePlus`, 18px).
- Rows are 56px: 36px accent avatar with an initial, bold 14px name,
  one-line 12px secondary preview, and an accent unread pill labelled
  "N unread messages". Hover and selected use `--op-surface-active`.
  Selection also sets `aria-current="location"`.

### Thread header

- 35px on chrome. The title is a bold 14px single line with ellipsis. In
  Settings it is an `h1` with the same size.
- `#connection-status` is a pill with a dot and text. It appears exactly
  once in the DOM and carries `data-connection-state` and
  `data-error-code`.
- `.header-actions` holds 28px borderless icon buttons (16px icons): New
  message window (`SquarePen`) and Open in composer window
  (`ExternalLink`).

### Security disclosures

- A row of muted pills (12px text, `--op-text-secondary`, 1px border) with
  12px icons: the sync state and "Carrier SMS/MMS not end-to-end
  encrypted". The row is always present in the main window (every view) and
  in the composer window. Do not tint it as a warning and do not remove it.

### Message bubbles

- `max-width: min(76%, 620px)`, 8px radius, 8×12 padding, 15px/1.5 body.
- Incoming: `--op-surface-active` with a `--op-border-strong` border.
  Outgoing (`.self`): accent fill, accent-text color, right-aligned.
- Footer: 11px timestamp plus status icon and the `STATUS_LABEL` text. On
  outgoing bubbles it uses `rgba(255,255,255,.75)`.
- Attachments render as `.file-card`: `FileText` or `Image` icon, name,
  human-readable size, and capitalised state. The public-link action keeps
  its plaintext warning visible.
- Timestamps are opaque display strings. Do not parse them.

### Composer

- `#composer-field`: one bordered 8px-radius container on
  `--op-surface-widget`. Focus shows on the container
  (`:focus-within`, accent border plus 1px ring). The textarea has no
  outline of its own.
- Inside, top to bottom: `#attachment-tray` (when present), then
  `#composer-input-row` with the textarea, then `#composer-toolbar`.
- Textarea: 15px/22px, auto-grows from 1 to 8 lines, then scrolls. When the
  user sizes the composer, the textarea fills the field.
- Placeholder: "Message {name}", falling back to "Type a message".
- Toolbar, in order: Add attachment (`Paperclip` 16px) · gateway control ·
  spacer · `#sms-counter` · newline hint · Send (`SendHorizontal` 18px).
  Send is accent-filled only when sendable.
- Newline hint: "⇧↵ new line" on macOS, "Shift+Enter new line" elsewhere.
  It shows only while the focused field has text.
- SMS counter: a length estimate. It appears from 85% of one segment and is
  hidden when attachments exist (MMS). GSM-7 allows 160 characters in one
  segment, 153 per segment when split; extension characters count 2. UCS-2
  allows 70 / 67 and counts UTF-16 code units. Formats: `142/160`, then
  `2 SMS · 145 left`.
- Gateway control (`#gateway-selector`, region "Gateway and SIM", select
  "Gateway"): a compact 24px select labelled "{gateway} · SIM n", plus
  " · Simulated" for simulated native gateways, and a `Radio` or
  `TriangleAlert` icon. When sending is blocked, `#unavailable-hint` shows
  "Sending unavailable: {reason}" in `--op-warning-text`, and the Send
  button references it with `aria-describedby`. When sending works the hint
  stays `hidden`.
- Enter sends; Shift+Enter inserts a newline. Ignore keys while
  `isComposing` (IME).

### Settings (`#settings-view`)

- Left-aligned column, max width 640px, 16px inset. Sections have
  `data-settings-section`, a 14px `h2`, and a 12px secondary description:
  Server, Device credentials, Sync encryption, Appearance, and Conversation
  heads.
- Rows use `.settings-control-row`: the label sits above the control and
  buttons stay inline. Selects and buttons are auto width (select
  min-width 160px). The server URL input uses the mono font.
- Only one Server URL input may be mounted at a time (Settings or
  onboarding).

### Buttons and inputs

| Class | Look |
|---|---|
| `.primary-button` | 28px, 0 12px padding, 4px radius, accent fill, accent-text; hover `--op-accent-hover` |
| `.secondary-button` | 28px, transparent, 1px `--op-border-strong`; hover `--op-surface-hover` |
| Icon button | 28×28, borderless, transparent, `--op-text-secondary`; hover `--op-surface-hover` |
| Input / select | 28px min height, 0 7px padding, 1px `--op-border`, 4px radius, `--op-surface-widget` |

Focus: `:focus-visible` uses a 2px `--op-focus` outline at a 2px offset.
Traffic lights use a 3px focus box-shadow; sashes use an inset 1px ring.

### Resize handle

- 1px structural line with an 8px invisible hit area (`::before`). The
  accent line (`::after`) and grip dots appear on hover, drag, and focus.
- Collapsed state: a 16px strip with a chevron, and title "Drag, click, or
  press Enter to restore pane".
- `role="separator"`, `aria-orientation`, and `aria-valuemin`, `-max`,
  `-now`, `-text` ("Collapsed" or "N pixels"). Keep `onResize` and
  `onResizeEnd` in refs; parent re-renders must not end a drag.

## 7. Icons

Use Lucide (`lucide-react`). Shared icons are re-exported from
`@openpush/desktop-ui`. Every icon is `aria-hidden`; the button carries
`aria-label` and a matching `title`.

| Size | Use |
|---|---|
| 10px | Windows caption glyphs |
| 12px | Disclosure pills, bubble status, attachment state |
| 14px | Gateway status, settings sync state |
| 16px | Header actions, attachment button |
| 18px | Send, New message |
| 20px | Rail items |

## 8. Accessibility rules

- Pair status color with an icon and text. Text meets WCAG AA (4.5:1) in
  both themes; check new surface/text pairs in light **and** dark.
- Give every icon-only button an `aria-label` and a `title`. Disabled
  decorative controls are `disabled`, `aria-hidden`, and `tabIndex=-1`.
- Do not give two landmarks the same name.
- Recipient combobox: keep `role="combobox"`, `aria-controls`,
  `aria-expanded`, and `aria-activedescendant`.
- Mark messages read only when they are visible (IntersectionObserver plus
  `data-message-id`). Presentation changes must not alter this.

## 9. Copy

- Use sentence case and short labels. Use verbs for actions ("Configure
  server", "Import credentials natively", "Unlock sync natively").
- Never show raw internal identifiers (SIM ids, UUIDs, file paths). Derive
  labels such as "SIM 1".
- Never show developer or debug text in the UI ("SIMULATED UI", "fixture",
  "Browser fixture…"). The browser fixture in `bridge.ts` uses realistic
  sample data and looks like the real app. The exception is the
  " · Simulated" gateway marker, which tells a tester that sends will not
  reach a carrier.
- The fixed security copy is "Carrier SMS/MMS not end-to-end encrypted",
  "Device sync encrypted", "Device sync key mismatch", and "Device sync not
  unlocked". Change it only deliberately and update the tests.

## 10. Code conventions

- **Hooks.** Every meaningful boundary gets a stable `id` (unique) or a
  semantic `data-*` attribute (repeated items: `data-conversation-id`,
  `data-message-id`, `data-attachment-id`, `data-rail-item`,
  `data-settings-section`, `data-disclosure`). Never use array indexes or
  presentation classes as identity.
- **Formatting.** Use readable multi-line TSX and CSS. Do not minify. Keep
  lines under about 160 characters. Do not reformat unrelated code in a
  presentation change.
- **CSS placement.** Put shared component styles in
  `packages/desktop-ui/src/styles.css` and shell or view styles in
  `apps/desktop/src/app.css`. Use one rule block per selector. Extend the
  existing block instead of appending an override later in the file.
- **`[hidden]`.** If a rule sets `display` on an element that can be
  `hidden`, add an explicit `[hidden] { display: none; }` rule.
- **Boundaries.** Presentation changes must not touch `DraftStore`,
  `resolveRoute`, `routeProblem`, send, save, or close logic, read marking,
  or the bridge DTO types. React never receives secrets.
- **Fonts and assets.** Bundle them locally with relative URLs; the CSP is
  `default-src 'self'`. Add no CDN or `@import url(...)`.
- **Dependencies.** Add none without need. Icons come from `lucide-react`
  only.

## 11. Verifying a UI change

1. Run the checks (Node 24 on `PATH`):

   ```sh
   pnpm --filter @openpush/desktop-ui typecheck
   pnpm --filter @openpush/desktop-ui test
   pnpm --filter @openpush/desktop typecheck
   pnpm --filter @openpush/desktop test
   pnpm --filter @openpush/desktop build
   ```

2. Start the fixture preview with
   `pnpm --filter @openpush/desktop dev --port 1420 --strictPort` and open
   `http://localhost:1420/`. Vite listens on `localhost`, not `127.0.0.1`.
3. Inspect at 1100×760 (main window) and 440×560
   (`?window=composer&conversationId=conv-aurora`) in dark and light.
4. Check Windows chrome on a Mac by overriding `navigator.userAgent` and
   `navigator.platform` (for example with Playwright `addInitScript`).
5. Exercise both sashes by pointer and keyboard. Reload to confirm
   persistence.
6. Add or update tests for any changed test-visible label, id, or role.

## 12. Known deviations

These are current limitations, not conventions to copy:

- `apps/desktop/src/app.css` repeats some selectors (`#settings-view`,
  `.primary-button`, `.secondary-button`, `.message-bubble`). Consolidate
  them when you next edit those blocks.
- Some shared rules in `styles.css` are written as single lines. Expand
  them when you edit them.
- The `@media` widths on `#thread-list` in `app.css` are overridden by the
  inline width that the sash sets.
- `GatewaySelector` numbers SIMs across the whole gateway list, not per
  gateway.
- Native OS notification banners are not implemented. Do not add UI that
  implies they exist.
