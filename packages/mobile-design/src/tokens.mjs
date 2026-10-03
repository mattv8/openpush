/**
 * Canonical mobile semantic names. Values are read from desktop-ui's CSS so
 * mobile colors remain parity-derived rather than independently maintained.
 */
export const semanticTokens = Object.freeze([
  "surface-desk", "surface-chrome", "surface-panel", "surface-editor",
  "surface-widget", "surface-hover", "surface-active", "surface-input",
  "border", "border-strong", "text-primary", "text-strong", "text-secondary",
  "text-muted", "accent", "accent-hover", "accent-text", "success", "error",
  "error-text", "warning-text", "focus", "focus-ring", "primary", "primary-text"
]);

export const kotlinNames = Object.freeze({
  "surface-desk": "surfaceDesk", "surface-chrome": "surfaceChrome",
  "surface-panel": "surfacePanel", "surface-editor": "surfaceEditor",
  "surface-widget": "surfaceWidget", "surface-hover": "surfaceHover",
  "surface-active": "surfaceActive", "surface-input": "surfaceInput",
  border: "border", "border-strong": "borderStrong", "text-primary": "textPrimary",
  "text-strong": "textStrong", "text-secondary": "textSecondary", "text-muted": "textMuted",
  accent: "accent", "accent-hover": "accentHover", "accent-text": "accentText",
  success: "success", error: "error", "error-text": "errorText", "warning-text": "warningText",
  focus: "focus", "focus-ring": "focusRing", primary: "primary", "primary-text": "primaryText"
});

export const swiftNames = Object.freeze(Object.fromEntries(
  Object.entries(kotlinNames).map(([key, value]) => [key, value[0].toUpperCase() + value.slice(1)])
));
