# Peppy mobile style guide

`packages/mobile-design` is the source and deterministic generator for shared
native design outputs. Run `pnpm --dir packages/mobile-design generate`; CI and
reviews use `pnpm --dir packages/mobile-design check` for drift.

## Generated native API

- Android imports `dev.peppy.mobile.ui.theme.PeppyTokens` and chooses
  `PeppyTokens.Dark` or `PeppyTokens.Light` from the system theme.
- iOS uses `PeppyTokens.colors(for: colorScheme)`. Its generated symbols use
  PascalCase semantic names (for example `Accent`, `SurfacePanel`).
- Android copy is `@string/peppy_<catalog_key>`; iOS copy is
  `String(localized: "peppy.<catalog_key>")`. Add shared copy only to
  `src/catalog.mjs`, never directly to platform output.

Use native typography and touch targets with a 4px spacing rhythm; do not copy
desktop control dimensions. Use the coral graphic brand and semantic coral accents; coral is not normal-size text, follow the
system light/dark appearance, and retain opaque readable content when reduced
transparency or motion is enabled. Use stable semantic native test identifiers.

For a changed shared mobile behavior, update and test Rust, bindings, Android,
and iOS when they are affected. State platform capability exceptions explicitly;
do not force meaningless edits to unaffected surfaces.
