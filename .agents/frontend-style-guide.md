# OpenPush desktop frontend style guide

Use this guide for the Tauri desktop UI: a dense, bounded workbench with
Pushbullet-style placement and Ragtime Modern as visual intent. It documents
UI contracts as well as presentation; behavior and source take precedence over
this guide. [Domain guidance](AGENTS.md) covers security, data, and evidence.

## Source ownership

Resolve a discrepancy from the implementation and its tests first, then the
token and style sources, then this guide. Ragtime is visual intent only; do
not import from it.

| Source | Owns |
|---|---|
| `packages/desktop-ui/src/op-tokens.css` | `--op-*` theme tokens, local Droid Sans, reduced motion |
| `packages/desktop-ui/src/styles.css` | Shared titlebar, panes, composer, and resize styling |
| `packages/desktop-ui/src/index.tsx` | `AppTitlebar`, shared desktop UI, composer, recipient panel |
| `packages/desktop-ui/src/phone.ts` | Shared phone validation, normalization, input and display formatting |
| `packages/desktop-ui/src/ResizeHandle.tsx` | Resizing behavior and handle semantics |
| `apps/desktop/src/App.tsx` | Shells, views, drafts, persistence, and titlebar status |
| `apps/desktop/src/app.css` | Desktop shell, conversation view, bubbles, and settings |

## Distinctive design and themes

Keep the rail, thread list, and conversation as docked workbench panes. Use
the compact 4px rhythm, tonal surfaces, and structural borders rather than a
generic web-page layout. The composer and contacts rail are deliberate
exceptions: both may float above the conversation with `--op-shadow-floating`.

All themeable color, surface, border, typography, spacing, radius, and shadow
values use explicit `--op-*` tokens. Shells carry `theme-system`,
`theme-light`, or `theme-dark`; light values intentionally appear in both
`.theme-light` and the system-light media rule. Legacy aliases in `styles.css`
are compatibility mappings, not new-code tokens.

Bundle fonts and assets locally with relative URLs. The CSP remains
`default-src 'self'`; do not introduce remote font or asset sources. Treat
display timestamps as opaque strings rather than values to parse or reformat.

## Titlebar status and chrome

`TitlebarStatus` supplies all three pills to `AppTitlebar` in both main and
composer windows: the sync state, the carrier disclosure, and connection
state. Connection and disclosures do not live in the conversation pane; there
is no separate security-disclosures row.

Keep this fixed text exactly: `Device sync encrypted`, `Device sync key
mismatch`, `Device sync not unlocked`, and `Carrier SMS/MMS not end-to-end
encrypted`. Labels may compact or hide responsively, while their `aria-label`
and `title` remain available.

The app draws platform-appropriate chrome so both windows share a recognizable
shell. Do not switch to native decorations: main-window and composer close
paths use `closeAfterSave`. A draft flush failure keeps the window open.

## Windows, drafts, and layout

The main window owns rail, list, and conversation views; the composer window
is a focused conversation surface without rail or thread list. The composer
must preserve draft recovery and close behavior rather than acting as a
second main window.

`openpush.layout.v1` is shared by both windows. Read it defensively, clamp
values while rendering, and persist only explicit user resize or rail position
changes. Merge only changed keys before writing so one window does not erase
the other's settings; never write back a value merely clamped for its current
viewport.

The composer overlays the conversation: `#message-list` reserves space from
`--composer-overlay-height`, while opaque gutters and its fade preserve legible
content beneath it. The visibility observer is rooted at that list and uses a
rounded negative bottom `rootMargin` for the overlay, recreating on height
changes without re-sending already seen message ids. Messages become read only
when actually visible.

The composer resize handle uses the supplied
`resize-handle resize-handle-vertical composer-resize-grip` `className`; that
class replaces default handle classes. Its card-top grip sizes the composer
without turning it into the list sash.

## Floating contacts rail

For a new conversation, `RecipientPanel` is a separate overlay inside
`#conversation-stage`, not a child of the composer. Its clipped
`#recipient-rail-layer` ends at the composer's top edge: downward movement
stops with the rail's bottom flush to that edge, never over the message input.
Default to the left edge with an 8px gap above that boundary. Drag freely in
the remaining conversation area; there are no corner anchors, docking rows,
or drop-target grids. Keep the panel mounted while moving so pending input
and focus survive.

Size the rail to its contents; its input grows and shrinks with its text and
placeholder. Wrap chips within the available width and scroll inside the rail
when its content exceeds the safe area's height. Observe the boundary layer
as well as the rail so composer resizing or validation errors re-clamp the
rail without saving window-only adjustments. The composer stacks above the
rail layer as a final protection against transient measurement changes.
Hide the composer's top fade while the rail is present so it does not dim the
rail's controls at the flush boundary; keep the fade in existing threads.

Persist exact `{ x, y }` pixels relative to the conversation area's top-left
as `recipientPosition` in the shared layout. Accept only finite nonnegative
coordinates. Ignore legacy `recipientAnchor`; remove it on the next explicit
rail move. Resize-only clamping never writes back the saved position. Pointer
cancellation reverts without saving. Grip arrows move 8px (Shift: 32px), with
no quantization of pointer movement. Keep the rail rounded, above the composer
in stacking order, and use `--op-shadow-floating` in both themes.

Recipient tokenization commits on Enter outside IME, delimiters, blur, and
separated paste. It trims and deduplicates committed tokens in first-occurrence
order, but typing alone does not commit; a paste's unseparated trailing text
remains in the input. Empty-input Backspace removes the last token.

Use the shared phone helpers for new recipient entry. Parsing defaults to US;
explicit international numbers remain supported. New numbers normalize to
E.164 IDs (for example, `+12025550123`) while US display uses national format
(`(202) 555-0123`). Keep existing synced IDs and contact names unchanged.
Validation checks numbering-plan structure, not ownership or SMS reachability.

Format the rail input while typing without changing IME composition or losing
the caret. Validate on commit, retain rejected text with an inline accessible
error, and never partially commit a mixed valid/invalid batch. Search remains
free text for existing contacts; only valid numbers offer a new conversation.
Pending recipient text blocks Send until committed or cleared. Chip removal
uses stored IDs; an explicit empty recipient list clears a saved draft rather
than restoring its previous recipients. Established replies still resolve
conversation recipients at send time.

## Contacts view

Keep contact books, contact search/list and contact detail as bounded workbench
panes. Use the existing theme tokens, controls and resize behavior. Each phone's
book remains identifiable; do not silently merge books or shared phone numbers.

Contact edits are requests to the owning phone. Show pending, conflict, approval,
expired and unavailable states without presenting an optimistic local edit as an
OS write that succeeded. Include the source account/container and read-only
field capabilities. Local forgetting hides a book; it never deletes OS contacts.

Resolve names and avatars for display without rewriting conversation IDs,
addresses, draft recipients or mirrored notification contents. Honor native
banner preview settings. Preserve a number-selection step for contacts with
multiple phone numbers and an initials fallback when a photo is unavailable.

Use stable contact/book identifiers for repeated component hooks. Keep photo
cropping keyboard-accessible and maintain focus through async save states.

## Native and fixture limits

The browser fixture uses realistic data, while ` · Simulated` identifies a
simulated native gateway only. Native banners depend on OS permission and app
installation, so the fixture cannot prove delivery or banner activation;
notification preview preferences remain distinct from encrypted-sync state.

Floating-head behavior needs the native input-region and focus integration;
keep the main-window and composer fallbacks. Do not infer native-host behavior
from browser CSS alone.
