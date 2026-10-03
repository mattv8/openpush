import type { KeyboardEvent, PointerEvent, ReactNode } from "react";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import {
  Check,
  Bell,
  Clock,
  ExternalLink,
  GripVertical,
  Loader,
  MessageCircle,
  MessageSquarePlus,
  Minus,
  Paperclip,
  SendHorizontal,
  Settings,
  Square,
  Users,
  X,
} from "lucide-react";
import {
  formatPhoneInput,
  formatPhoneNumber,
  normalizePhoneNumber,
} from "./phone";
export { ResizeHandle, type ResizeHandleProps } from "./ResizeHandle";
export { installOverlayScrollbars, OVERLAY_SCROLL_HIDE_DELAY } from "./overlayScroll";
export {
  formatPhoneInput,
  formatPhoneNumber,
  isValidPhoneNumber,
  normalizePhoneNumber,
} from "./phone";
import "./styles.css";
export { ContactAvatar } from "./ContactAvatar";
export {
  Bell,
  Check,
  CheckCheck,
  CheckCircle,
  Clock,
  ExternalLink,
  FileText,
  FlaskConical,
  Image,
  Lock,
  LockKeyhole,
  LockOpen,
  MessageSquare,
  MessageSquarePlus,
  Radio,
  ShieldAlert,
  SquarePen,
  TriangleAlert,
  X,
} from "lucide-react";

export const tokens = {
  sidebarWidth: 280,
  rowHeight: 56,
  avatarSize: 36,
  messageFont: "15px",
  space: { xs: 4, sm: 8, md: 12, lg: 16, xl: 24 },
} as const;
/** Deprecated inline-token bridge retained for downstream compatibility. */
export const themeTokens = {
  light: { "--surface-0": "#fff" },
  dark: { "--surface-0": "#1f1f1f" },
} as const;
export type Conversation = {
  id: string;
  name: string;
  /** Display-only contact photo (data URL); initials are shown otherwise. */
  avatarUrl?: string;
  preview: string;
  unread: number;
  status?: string;
};
export type Attachment = {
  id: string;
  name: string;
  state: "pending" | "ready" | "uploading" | "failed";
  error?: string;
  previewUrl?: string;
};

export type RecipientChip = { id: string; label: string; avatarUrl?: string };
/** A contact phone offered while typing; `id` is the phone address that becomes the recipient. */
export type RecipientSuggestion = { id: string; label: string; detail?: string; avatarUrl?: string };
export type RecipientPosition = { x: number; y: number };

export function isRecipientPosition(value: unknown): value is RecipientPosition {
  return typeof value === "object" && value !== null
    && Number.isFinite((value as RecipientPosition).x) && (value as RecipientPosition).x >= 0
    && Number.isFinite((value as RecipientPosition).y) && (value as RecipientPosition).y >= 0;
}

function recipientAvatarLetter(label: string): string {
  if (/^[+\d]/.test(label.trim())) return "#";
  return label.match(/[a-z0-9]/i)?.[0]?.toUpperCase() ?? "#";
}

function recipientLabel(label: string): string {
  return normalizePhoneNumber(label) ? formatPhoneNumber(label) : label;
}

function selectionForDigitCount(value: string, digits: number): number {
  if (!digits) return 0;
  let seen = 0;
  for (let index = 0; index < value.length; index += 1) {
    if (/\d/.test(value[index])) seen += 1;
    if (seen === digits) return index + 1;
  }
  return value.length;
}

export function RecipientPanel({
  recipients,
  onCommit,
  position,
  onPositionChange,
  hint,
  bottomOffset = 8,
  onPendingChange,
  searchContacts,
  onSuggestionChosen,
}: {
  recipients: RecipientChip[];
  onCommit(ids: string[]): void;
  position: RecipientPosition | null;
  onPositionChange(position: RecipientPosition): void;
  hint?: string;
  bottomOffset?: number;
  onPendingChange?(pending: boolean): void;
  /** Contact discovery: phone numbers (not contact IDs) matching a name or digits. */
  searchContacts?(query: string): Promise<RecipientSuggestion[]>;
  onSuggestionChosen?(suggestion: RecipientSuggestion): void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  const panelRef = useRef<HTMLElement>(null);
  const boundaryRef = useRef<HTMLElement>(null);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [failedAvatars, setFailedAvatars] = useState<Set<string>>(() => new Set());
  const [dimensions, setDimensions] = useState({ boundaryWidth: 0, boundaryHeight: 0, panelWidth: 0, panelHeight: 0 });
  const [livePosition, setLivePosition] = useState<RecipientPosition | null>(null);
  const [dragging, setDragging] = useState(false);
  const latestCommittedPositionRef = useRef<RecipientPosition | null>(null);
  const controlledPositionRef = useRef<RecipientPosition | null>(null);
  const dragRef = useRef<{
    pointerId: number;
    startX: number;
    startY: number;
    startPosition: RecipientPosition;
    clientX: number;
    clientY: number;
  } | null>(null);
  const pendingSelectionRef = useRef<number | null>(null);
  const skipBlurCommitRef = useRef(false);
  const [suggestions, setSuggestions] = useState<RecipientSuggestion[]>([]);
  const [activeSuggestion, setActiveSuggestion] = useState(0);
  const searchSeqRef = useRef(0);
  const suggestionListId = useId();
  const recipientIds = recipients.map((recipient) => recipient.id).join("\n");
  // Latest callback without re-running the search on every parent render.
  const searchRef = useRef(searchContacts);
  searchRef.current = searchContacts;
  const canSearch = Boolean(searchContacts);
  useEffect(() => {
    const query = value.trim();
    const request = ++searchSeqRef.current;
    const search = searchRef.current;
    if (!canSearch || !search || !query) {
      setSuggestions([]);
      return;
    }
    const timer = setTimeout(() => {
      void search(query)
        .then((found) => {
          if (request !== searchSeqRef.current) return;
          const chosen = new Set(recipientIds.split("\n"));
          setSuggestions(found.filter((item) => !chosen.has(item.id)).slice(0, 20));
          setActiveSuggestion(0);
        })
        .catch(() => {
          if (request === searchSeqRef.current) setSuggestions([]);
        });
    }, 120);
    return () => clearTimeout(timer);
  }, [canSearch, value, recipientIds]);
  const chooseSuggestion = (suggestion: RecipientSuggestion) => {
    searchSeqRef.current += 1;
    setSuggestions([]);
    onSuggestionChosen?.(suggestion);
    if (!recipients.some((recipient) => recipient.id === suggestion.id)) {
      onCommit([...recipients.map((recipient) => recipient.id), suggestion.id]);
    }
    setError(null);
    setValue("");
    inputRef.current?.focus();
  };

  useEffect(() => {
    onPendingChange?.(Boolean(value));
  }, [onPendingChange, value]);
  useLayoutEffect(() => {
    const selection = pendingSelectionRef.current;
    if (selection === null || !inputRef.current) return;
    inputRef.current.setSelectionRange(selection, selection);
    pendingSelectionRef.current = null;
  }, [value]);

  const boundaryForPanel = (panel = panelRef.current) => panel?.closest<HTMLElement>("#recipient-rail-layer")
    ?? panel?.closest<HTMLElement>("#conversation-stage");
  const measure = () => {
    const panel = panelRef.current;
    const boundary = boundaryForPanel(panel);
    if (!panel || !boundary) return;
    boundaryRef.current = boundary;
    const boundaryRect = boundary.getBoundingClientRect();
    const panelRect = panel.getBoundingClientRect();
    const next = {
      boundaryWidth: boundaryRect.width,
      boundaryHeight: boundaryRect.height,
      panelWidth: panelRect.width,
      panelHeight: panelRect.height,
    };
    if (Object.values(next).every(Number.isFinite)) {
      setDimensions((current) => Object.keys(next).every((key) => current[key as keyof typeof current] === next[key as keyof typeof next]) ? current : next);
    }
  };
  useLayoutEffect(() => {
    measure();
    const boundary = boundaryRef.current;
    const panel = panelRef.current;
    if (!boundary || !panel || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(boundary);
    observer.observe(panel);
    return () => observer.disconnect();
  }, [bottomOffset, recipients.length]);

  const hasBounds = (current = dimensions) => current.boundaryWidth > 0 && current.boundaryHeight > 0 && current.panelWidth > 0 && current.panelHeight > 0;
  const clamp = (candidate: RecipientPosition, current = dimensions): RecipientPosition => !hasBounds(current) ? candidate : {
    x: Math.min(Math.max(candidate.x, 0), Math.max(current.boundaryWidth - current.panelWidth, 0)),
    y: Math.min(Math.max(candidate.y, 0), Math.max(current.boundaryHeight - current.panelHeight, 0)),
  };
  const defaultPosition = hasBounds()
    ? clamp({ x: 12, y: dimensions.boundaryHeight - bottomOffset - dimensions.panelHeight })
    : { x: 12, y: 0 };
  if (position && (position.x !== controlledPositionRef.current?.x || position.y !== controlledPositionRef.current?.y)) {
    controlledPositionRef.current = { ...position };
    latestCommittedPositionRef.current = { ...position };
  }
  const displayedPosition = clamp(livePosition ?? position ?? defaultPosition);
  const currentBounds = () => {
    const panel = panelRef.current;
    const boundary = boundaryForPanel(panel);
    if (!panel || !boundary) return dimensions;
    boundaryRef.current = boundary;
    const boundaryRect = boundary.getBoundingClientRect();
    const panelRect = panel.getBoundingClientRect();
    return boundaryRect.width > 0 && boundaryRect.height > 0 && panelRect.width > 0 && panelRect.height > 0
      ? { boundaryWidth: boundaryRect.width, boundaryHeight: boundaryRect.height, panelWidth: panelRect.width, panelHeight: panelRect.height }
      : dimensions;
  };

  const commitTokens = (text: string, keepTrailing = false) => {
    const pieces = text.split(/[,;\n]/);
    const trailing = keepTrailing && !/[,;\n]$/.test(text) ? pieces.pop() ?? "" : "";
    if (trailing && /[^\d\s().+-]/.test(trailing)) {
      setError("Enter a complete phone number.");
      setValue(text);
      return;
    }
    const newIds = pieces.map((piece) => piece.trim()).filter(Boolean);
    const normalized = newIds.map(normalizePhoneNumber);
    if (normalized.some((id) => !id)) {
      setError("Enter a complete phone number.");
      setValue(text);
      return;
    }
    const existingIds = recipients.map((recipient) => recipient.id);
    const seen = new Set(existingIds.map((id) => normalizePhoneNumber(id) ?? id));
    const newIdsToAppend = (normalized as string[]).filter((id) => {
      if (seen.has(id)) return false;
      seen.add(id);
      return true;
    });
    const next = [...existingIds, ...newIdsToAppend];
    if (next.length !== recipients.length || next.some((id, index) => id !== recipients[index]?.id)) {
      onCommit(next);
    }
    setError(null);
    setValue(trailing.trim());
  };
  const updateValue = (nextValue: string, selectionStart: number | null, composing = false) => {
    if (composing) {
      setValue(nextValue);
      return;
    }
    const formatted = formatPhoneInput(nextValue);
    if (formatted !== nextValue && selectionStart !== null) {
      pendingSelectionRef.current = selectionForDigitCount(formatted, (nextValue.slice(0, selectionStart).match(/\d/g) ?? []).length);
    }
    setError(null);
    setValue(formatted);
  };
  const onGripPointerDown = (event: PointerEvent<HTMLButtonElement>) => {
    if (event.button && event.button !== 0) return;
    const panel = panelRef.current;
    const boundary = boundaryForPanel(panel);
    if (!panel || !boundary) return;
    boundaryRef.current = boundary;
    event.currentTarget.setPointerCapture?.(event.pointerId);
    dragRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      startPosition: displayedPosition,
      clientX: event.clientX,
      clientY: event.clientY,
    };
    setDragging(true);
  };
  const onGripPointerMove = (event: PointerEvent<HTMLButtonElement>) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    drag.clientX = event.clientX;
    drag.clientY = event.clientY;
    setLivePosition(clamp({ x: drag.startPosition.x + event.clientX - drag.startX, y: drag.startPosition.y + event.clientY - drag.startY }, currentBounds()));
  };
  const finishDrag = (event: PointerEvent<HTMLButtonElement>, cancelled = false) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    drag.clientX = event.clientX;
    drag.clientY = event.clientY;
    const next = clamp({ x: drag.startPosition.x + drag.clientX - drag.startX, y: drag.startPosition.y + drag.clientY - drag.startY }, currentBounds());
    event.currentTarget.releasePointerCapture?.(event.pointerId);
    dragRef.current = null;
    setDragging(false);
    setLivePosition(null);
    if (!cancelled && (next.x !== drag.startPosition.x || next.y !== drag.startPosition.y)) {
      latestCommittedPositionRef.current = next;
      onPositionChange(next);
    }
  };
  const gripKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    const amount = event.shiftKey ? 32 : 8;
    const delta = event.key === "ArrowUp" ? { x: 0, y: -amount }
      : event.key === "ArrowDown" ? { x: 0, y: amount }
      : event.key === "ArrowLeft" ? { x: -amount, y: 0 }
      : event.key === "ArrowRight" ? { x: amount, y: 0 }
      : null;
    if (!delta) return;
    event.preventDefault();
    const base = latestCommittedPositionRef.current ?? displayedPosition;
    const next = clamp({ x: base.x + delta.x, y: base.y + delta.y }, currentBounds());
    latestCommittedPositionRef.current = next;
    onPositionChange(next);
  };

  return <section
      id="draft-recipients"
      ref={panelRef}
      aria-label="Message recipients"
      data-dragging={dragging ? "true" : undefined}
      style={{ left: displayedPosition.x, top: displayedPosition.y }}
    >
      <button
        id="recipient-panel-grip"
        type="button"
        aria-label="Move recipients panel"
        title="Move recipients panel"
        onPointerDown={onGripPointerDown}
        onPointerMove={onGripPointerMove}
        onPointerUp={finishDrag}
        onPointerCancel={(event) => finishDrag(event, true)}
        onKeyDown={gripKeyDown}
      >
        <GripVertical size={14} aria-hidden />
        <span className="sr-only">Use arrow keys to move recipients; hold Shift to move further.</span>
      </button>
      <span className="recipient-label" aria-hidden>To</span>
      <ul id="recipient-chips" aria-label="Recipient list">
        {recipients.map((recipient) => {
          const showImage = recipient.avatarUrl && !failedAvatars.has(recipient.id);
          return <li key={recipient.id} data-recipient-id={recipient.id} className="recipient-chip">
            <span className="recipient-avatar" aria-hidden>
              {showImage ? <img src={recipient.avatarUrl} alt="" onError={() => setFailedAvatars((current) => new Set(current).add(recipient.id))} /> : recipientAvatarLetter(recipient.label)}
            </span>
            <span className="recipient-chip-label">{recipientLabel(recipient.label)}</span>
            <button
              type="button"
              data-recipient-remove
              aria-label={`Remove ${recipientLabel(recipient.label)}`}
              title={`Remove ${recipientLabel(recipient.label)}`}
              onPointerDown={() => { skipBlurCommitRef.current = true; }}
              onClick={() => {
                onCommit(recipients.filter((item) => item.id !== recipient.id).map((item) => item.id));
                inputRef.current?.focus();
                skipBlurCommitRef.current = false;
              }}
            >
              <X size={12} aria-hidden />
            </button>
          </li>;
        })}
      </ul>
      <input
        ref={inputRef}
        aria-label="Recipients"
        aria-controls={suggestions.length ? suggestionListId : undefined}
        aria-activedescendant={suggestions.length ? `${suggestionListId}-${activeSuggestion}` : undefined}
        aria-autocomplete={searchContacts ? "list" : undefined}
        aria-describedby={`draft-recipients-hint${error ? " draft-recipients-error" : ""}`}
        aria-invalid={error ? "true" : undefined}
        autoComplete="tel"
        inputMode="tel"
        placeholder={recipients.length ? "Add" : "Add phone number"}
        size={Math.min(Math.max(value.length, recipients.length ? 3 : 16), 24)}
        value={value}
        onChange={(event) => updateValue(event.target.value, event.target.selectionStart, (event.nativeEvent as InputEvent).isComposing)}
        onKeyDown={(event) => {
          if (event.nativeEvent.isComposing) return;
          if (suggestions.length && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
            event.preventDefault();
            setActiveSuggestion((current) => event.key === "ArrowDown"
              ? Math.min(current + 1, suggestions.length - 1)
              : Math.max(current - 1, 0));
          } else if (suggestions.length && event.key === "Escape") {
            event.preventDefault();
            searchSeqRef.current += 1;
            setSuggestions([]);
          } else if (event.key === "Enter" && suggestions.length && !normalizePhoneNumber(value)) {
            // A typed complete number still commits itself; names pick the highlighted phone.
            event.preventDefault();
            chooseSuggestion(suggestions[Math.min(activeSuggestion, suggestions.length - 1)]);
          } else if (event.key === "Enter") {
            event.preventDefault();
            commitTokens(value);
          } else if (event.key === "," || event.key === ";") {
            event.preventDefault();
            commitTokens(`${value}${event.key}`);
          } else if (event.key === "Backspace" && !value && recipients.length) {
            onCommit(recipients.slice(0, -1).map((recipient) => recipient.id));
          } else if (event.key === "Backspace" && event.currentTarget.selectionStart === event.currentTarget.selectionEnd && event.currentTarget.selectionStart && !/\d/.test(value[event.currentTarget.selectionStart - 1])) {
            event.preventDefault();
            let digitIndex = event.currentTarget.selectionStart - 1;
            while (digitIndex >= 0 && !/\d/.test(value[digitIndex])) digitIndex -= 1;
            if (digitIndex < 0) return;
            updateValue(`${value.slice(0, digitIndex)}${value.slice(digitIndex + 1)}`, digitIndex);
          }
        }}
        onBlur={(event) => {
          const nextTarget = event.relatedTarget as HTMLElement | null;
          if (suggestions.length && !normalizePhoneNumber(value) && /[^\d\s().+-]/.test(value)) {
            // A partial name is search text, not a number to commit.
            return;
          }
          if (skipBlurCommitRef.current || nextTarget?.matches("[data-recipient-remove]")) {
            skipBlurCommitRef.current = false;
            return;
          }
          commitTokens(value);
        }}
        onPaste={(event) => {
          const pasted = event.clipboardData.getData("text");
          if (!/[,;\n]/.test(pasted)) return;
          event.preventDefault();
          commitTokens(`${value}${pasted}`, true);
        }}
      />
      {suggestions.length > 0 && (
        <ul id={suggestionListId} role="listbox" aria-label="Contact suggestions" className="recipient-suggestions" data-recipient-suggestions>
          {suggestions.map((suggestion, index) => (
            <li
              key={`${suggestion.id}-${index}`}
              id={`${suggestionListId}-${index}`}
              role="option"
              aria-selected={index === activeSuggestion}
              data-suggestion-address={suggestion.id}
              onMouseDown={(event) => {
                event.preventDefault();
                chooseSuggestion(suggestion);
              }}
            >
              <span className="recipient-avatar" aria-hidden>
                {suggestion.avatarUrl ? <img src={suggestion.avatarUrl} alt="" /> : recipientAvatarLetter(suggestion.label)}
              </span>
              <span className="recipient-suggestion-copy">
                <b>{suggestion.label}</b>
                {suggestion.detail && <small>{suggestion.detail}</small>}
              </span>
            </li>
          ))}
        </ul>
      )}
      {recipients.length > 1 && <span className="recipient-group-tag" data-recipient-group>Group · MMS</span>}
      <span id="draft-recipients-hint" className="sr-only">{hint}</span>
      {error && <span id="draft-recipients-error" className="recipient-error" role="alert">{error}</span>}
    </section>;
}

function smsCounter(text: string): string | null {
  if (!text) return null;
  const gsmBasic = new Set(
    "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞ\u001bÆæßÉ " +
      "!\"#¤%&'()*+,-./0123456789:;<=>?¡" +
      "ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿" +
      "abcdefghijklmnopqrstuvwxyzäöñüà",
  );
  const gsmExtension = new Set("^{}\\[~]|€\f");
  const gsm7 = Array.from(text).every(
    (character) => gsmBasic.has(character) || gsmExtension.has(character),
  );
  const single = gsm7 ? 160 : 70;
  const multi = gsm7 ? 153 : 67;
  const units = gsm7
    ? Array.from(text).reduce(
        (total, character) => total + (gsmExtension.has(character) ? 2 : 1),
        0,
      )
    : text.length;
  if (units < Math.floor(single * 0.85)) return null;
  if (units <= single) return `${units}/${single}`;
  const segments = Math.ceil(units / multi);
  const remaining = segments * multi - units;
  return `${segments} SMS · ${remaining} left`;
}

export function detectPlatform(): "macos" | "windows" | "linux" {
  if (typeof navigator === "undefined") return "linux";
  const source = navigator.platform ?? navigator.userAgent;
  return /Mac/.test(source) ? "macos" : /Win/.test(source) ? "windows" : "linux";
}

export function AppTitlebar({
  onMinimize,
  onMaximize,
  onClose,
  platform,
  isComposer = false,
  title,
  status,
}: {
  onMinimize(): void;
  onMaximize(): void;
  onClose(): void;
  simulated?: boolean;
  platform?: "macos" | "windows" | "linux";
  isComposer?: boolean;
  title?: string;
  status?: ReactNode;
}) {
  const macos = (platform ?? detectPlatform()) === "macos";
  return (
    <header
      id="desktop-titlebar"
      className="titlebar"
      data-tauri-drag-region
      role="banner"
      aria-label="Peppy title bar"
      data-composer={isComposer || undefined}
    >
      {macos && <div
        id="window-controls"
        className="window-controls window-controls-macos"
        role="toolbar"
        aria-label="Window controls"
      >
        <button
          className="traffic-light traffic-light-close"
          aria-label={isComposer ? "Close composer" : "Close window"}
          title={isComposer ? "Close composer" : "Close window"}
          onClick={onClose}
        />
        <button
          className="traffic-light traffic-light-minimize"
          aria-label="Minimize window"
          title="Minimize window"
          onClick={onMinimize}
          disabled={isComposer}
          aria-hidden={isComposer || undefined}
          tabIndex={isComposer ? -1 : undefined}
        />
        <button
          className="traffic-light traffic-light-maximize"
          aria-label="Maximize window"
          title="Maximize window"
          onClick={onMaximize}
          disabled={isComposer}
          aria-hidden={isComposer || undefined}
          tabIndex={isComposer ? -1 : undefined}
        />
      </div>}
      <span className="titlebar-product" aria-hidden>
        ◈
      </span>
      <strong className="titlebar-wordmark" data-tauri-drag-region>
        {isComposer ? (
          <b className="titlebar-conversation-title">
            {title ?? "Compose message"}
          </b>
        ) : "Peppy"}
      </strong>
      <div className="titlebar-spacer" data-tauri-drag-region />
      {status && <div id="titlebar-status" className="titlebar-status">{status}</div>}
      {!macos && <div
        id="window-controls"
        className="window-controls window-controls-windows"
        role="toolbar"
        aria-label="Window controls"
      >
        {!isComposer && (
          <>
            <button
              aria-label="Minimize window"
              title="Minimize window"
              onClick={onMinimize}
            >
              <Minus size={10} aria-hidden />
            </button>
            <button
              aria-label="Maximize window"
              title="Maximize window"
              onClick={onMaximize}
            >
              <Square size={10} aria-hidden />
            </button>
          </>
        )}
        <button
          aria-label={isComposer ? "Close composer" : "Close window"}
          title={isComposer ? "Close composer" : "Close window"}
          onClick={onClose}
          className="close-button"
        >
          <X size={10} aria-hidden />
        </button>
      </div>}
    </header>
  );
}

export function ConversationList({
  conversations,
  selectedId,
  onSelect,
  onPopout,
  loading = false,
}: {
  conversations: Conversation[];
  selectedId: string;
  onSelect(id: string): void;
  onPopout?(id: string): void;
  loading?: boolean;
}) {
  return (
    <nav id="conversation-list" aria-label="Conversations" aria-busy={loading}>
      <ul role="list">
        {loading ? (
          <li className="sidebar-empty">Loading conversations…</li>
        ) : conversations.length ? (
          conversations.map((c) => (
            <li key={c.id}>
              <div className={`conversation-row ${selectedId === c.id ? "selected" : ""}`} data-conversation-row={c.id}>
              <button data-conversation-id={c.id} className="conversation-select" aria-current={selectedId === c.id ? "location" : undefined} onClick={() => onSelect(c.id)}>
                <span className="avatar" aria-hidden>
                  {c.avatarUrl ? <img src={c.avatarUrl} alt="" /> : c.name.slice(0, 1).toUpperCase()}
                </span>
                <span className="conversation-copy">
                  <b>{c.name}</b>
                  <small>{c.preview}</small>
                </span>
                {c.unread > 0 && (
                  <span
                    aria-label={`${c.unread} unread messages`}
                    className="unread"
                  >
                    <span aria-hidden>{c.unread}</span>
                  </span>
                )}
              </button>
              {onPopout && <button type="button" className="conversation-popout" data-popout-conversation-id={c.id} aria-label="Open as floating conversation" title={`Open ${c.name} as floating conversation`} onClick={() => onPopout(c.id)}><ExternalLink size={15} aria-hidden /></button>}
              </div>
            </li>
          ))
        ) : (
          <li className="sidebar-empty">No conversations yet</li>
        )}
      </ul>
    </nav>
  );
}

type PickerOption =
  | { kind: "existing"; id: string; name: string }
  | { kind: "contact"; id: string; name: string; detail?: string; avatarUrl?: string; value: string }
  | { kind: "new"; id: string; name: string; value: string };
const NEW_OPTION_ID = "new-recipient";
export function RecipientPicker({
  recipients,
  onChange,
  onNewRecipient,
  searchContacts,
}: {
  recipients: Conversation[];
  onChange(ids: string[]): void;
  onNewRecipient?(value: string): void;
  /** Contacts without a conversation: each phone number is its own option (its address). */
  searchContacts?(query: string): Promise<RecipientSuggestion[]>;
}) {
  const [query, setQuery] = useState("");
  const [contactMatches, setContactMatches] = useState<RecipientSuggestion[]>([]);
  const contactSeq = useRef(0);
  const searchRef = useRef(searchContacts);
  searchRef.current = searchContacts;
  const canSearch = Boolean(searchContacts && onNewRecipient);
  useEffect(() => {
    const text = query.trim();
    const request = ++contactSeq.current;
    const search = searchRef.current;
    if (!canSearch || !search || !text) {
      setContactMatches([]);
      return;
    }
    const timer = setTimeout(() => {
      void search(text)
        .then((found) => {
          if (request === contactSeq.current) setContactMatches(found.slice(0, 20));
        })
        .catch(() => {
          if (request === contactSeq.current) setContactMatches([]);
        });
    }, 120);
    return () => clearTimeout(timer);
  }, [canSearch, query]);
  const [chosen, setChosen] = useState<string[]>([]);
  const [active, setActive] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const listId = useId();
  const errorId = useId();
  const typed = query.trim();
  const normalizedQuery = normalizePhoneNumber(typed);
  const options: PickerOption[] = [
    ...recipients
      .filter(
        (r) =>
          (r.name.toLowerCase().includes(query.toLowerCase()) ||
            (normalizedQuery !== null && normalizePhoneNumber(r.name) === normalizedQuery)) &&
          !chosen.includes(r.id),
      )
      .map((r) => ({ kind: "existing" as const, id: r.id, name: r.name })),
    ...contactMatches.map((match, index) => ({
      kind: "contact" as const,
      id: `contact-${index}`,
      name: match.label,
      detail: match.detail,
      avatarUrl: match.avatarUrl,
      value: match.id,
    })),
    ...(onNewRecipient && normalizedQuery && !contactMatches.some((match) => match.id === normalizedQuery)
      ? [
          {
            kind: "new" as const,
            id: NEW_OPTION_ID,
            name: `Message ${formatPhoneNumber(normalizedQuery)}`,
            value: normalizedQuery,
          },
        ]
      : []),
  ];
  const reset = () => {
    setQuery("");
    setActive(0);
  };
  const commit = (option: PickerOption) => {
    if (option.kind === "new" || option.kind === "contact") {
      onNewRecipient?.(option.value);
      setError(null);
      reset();
      return;
    }
    const next = [...chosen, option.id];
    setChosen(next);
    onChange(next);
    reset();
  };
  const keydown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing) return;
    if (event.key === "ArrowDown") {
      if (!options.length) return;
      event.preventDefault();
      setActive((x) => Math.min(x + 1, options.length - 1));
    } else if (event.key === "ArrowUp") {
      if (!options.length) return;
      event.preventDefault();
      setActive((x) => Math.max(x - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (options.length) commit(options[Math.max(0, Math.min(active, options.length - 1))]);
      else if (typed) setError("Enter a complete phone number.");
    } else if (event.key === "Escape") setQuery("");
  };
  return (
    <section
      id="recipient-picker"
      className="recipient-picker"
      aria-label="New message recipients"
    >
      <div className="chips">
        {chosen.map((id) => {
          const r = recipients.find((x) => x.id === id);
          return (
            <button
              key={id}
              className="chip"
              data-recipient-id={id}
              onClick={() => {
                const next = chosen.filter((x) => x !== id);
                setChosen(next);
                onChange(next);
              }}
              aria-label={`Remove ${r?.name ?? id}`}
            >
              {r?.name ?? id} ×
            </button>
          );
        })}
      </div>
      <input
        id="recipient-search"
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={Boolean(query) && options.length > 0}
        aria-haspopup="listbox"
        aria-controls={listId}
        aria-activedescendant={
          query && options[active]
            ? `${listId}-${options[active].id}`
            : undefined
        }
        aria-label="Search recipients"
        aria-invalid={error ? "true" : undefined}
        aria-describedby={error ? errorId : undefined}
        value={query}
        onKeyDown={keydown}
        onChange={(e) => {
          setQuery(e.target.value);
          setActive(0);
          setError(null);
        }}
        placeholder="Search or start new"
      />
      {error && <span id={errorId} className="recipient-picker-error" role="alert">{error}</span>}
      {query && (
        <ul id={listId} role="listbox">
          {options.map((option, index) => (
            <li
              id={`${listId}-${option.id}`}
              role="option"
              aria-selected={index === active}
              key={option.id}
              data-recipient-id={
                option.kind === "existing" ? option.id : undefined
              }
              data-new-recipient={option.kind === "new" ? "true" : undefined}
              data-contact-address={option.kind === "contact" ? option.value : undefined}
              onMouseDown={(e) => {
                e.preventDefault();
                commit(option);
              }}
            >
              {option.kind === "contact" ? (
                <>
                  <span className="recipient-avatar" aria-hidden>
                    {option.avatarUrl ? <img src={option.avatarUrl} alt="" /> : recipientAvatarLetter(option.name)}
                  </span>
                  <span className="recipient-suggestion-copy">
                    <b>{option.name}</b>
                    {option.detail && <small>{option.detail}</small>}
                  </span>
                </>
              ) : option.name}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function Composer({
  draft,
  attachments,
  sendSupported,
  onDraftChange,
  onSend,
  status,
  onAddAttachment,
  onRemoveAttachment,
  unavailableReason,
  gatewaySlot,
  bannerSlot,
  statusSlot,
  statusActive = false,
  composerName,
  composerUserSized = false,
  maxAutoGrowHeight = 176,
  platform,
}: {
  draft: string;
  attachments: Attachment[];
  sendSupported: boolean;
  onDraftChange(value: string): void;
  onSend(): void;
  status?: string;
  onAddAttachment?(): void;
  onRemoveAttachment?(id: string): void;
  unavailableReason?: string;
  gatewaySlot?: ReactNode;
  bannerSlot?: ReactNode;
  statusSlot?: ReactNode;
  statusActive?: boolean;
  composerName?: string;
  composerUserSized?: boolean;
  /** CSS max-height: 176px remains the hard ceiling (8 lines); this can only lower auto-grow. */
  maxAutoGrowHeight?: number;
  platform?: "macos" | "windows" | "linux";
}) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const hasContent = Boolean(draft.trim()) || attachments.length > 0;
  const canSend = sendSupported && hasContent;
  const counter = !attachments.length ? smsCounter(draft) : null;
  const fallbackShown = !sendSupported && !gatewaySlot && !statusSlot;
  useEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea || composerUserSized) return;
    textarea.style.height = "0";
    textarea.style.height = `${Math.min(textarea.scrollHeight, maxAutoGrowHeight)}px`;
  }, [composerUserSized, draft, maxAutoGrowHeight]);
  const keydown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.nativeEvent.isComposing
    ) {
      event.preventDefault();
      if (canSend) onSend();
    }
  };
  const icon = (state: Attachment["state"]) =>
    state === "ready" ? (
      <Check size={12} />
    ) : state === "failed" ? (
      <X size={12} />
    ) : state === "uploading" ? (
      <Loader size={12} />
    ) : (
      <Clock size={12} />
    );
  return (
    <section
      id="shared-composer"
      className="composer"
      aria-label="Message composer"
    >
      <div id="composer-field">
        <div id="composer-banner-row" hidden={!bannerSlot}>
          {bannerSlot}
        </div>
        {attachments.length > 0 && (
        <ul id="attachment-tray" aria-label="Attachments">
          {attachments.map((a) => (
            <li
              key={a.id}
              data-attachment-id={a.id}
              className={`attachment-chip ${a.state}`}
            >
              {a.previewUrl && (
                <img
                  src={a.previewUrl}
                  alt={a.name}
                  onError={(e) => {
                    e.currentTarget.hidden = true;
                  }}
                />
              )}
              <span>{a.name}</span>
              <span className="attachment-state" aria-label={a.state}>
                {icon(a.state)}
              </span>
               {a.error && <span role="alert">{a.error}</span>}
               {onRemoveAttachment && (
                 <button
                   type="button"
                   className="attachment-remove"
                   aria-label={`Remove ${a.name}`}
                   title={`Remove ${a.name}`}
                   onClick={() => onRemoveAttachment(a.id)}
                 >
                   <X size={12} aria-hidden />
                 </button>
               )}
            </li>
          ))}
        </ul>
      )}
        <div id="composer-input-row">
        <label htmlFor="composer-textarea" className="sr-only">
          Message
        </label>
        <textarea
          ref={textareaRef}
          id="composer-textarea"
          aria-label="Message"
          value={draft}
          onChange={(e) => onDraftChange(e.target.value)}
          onKeyDown={keydown}
          placeholder={
            composerName ? `Message ${composerName}` : "Type a message"
          }
          rows={1}
        />
      </div>
        <div
          id="composer-status-row"
          hidden={!(statusActive || status || fallbackShown)}
        >
          {statusSlot}
          {status && (
            <p className="composer-status" role="status">
              {status}
            </p>
          )}
          {fallbackShown && <span id="unavailable-hint">Sending unavailable: {unavailableReason ?? "gateway is offline"}</span>}
        </div>
        <div id="composer-toolbar">
        <button
          type="button"
          aria-label="Add attachment"
          title="Add attachment"
          onClick={onAddAttachment}
          disabled={!onAddAttachment}
        >
          <Paperclip size={16} aria-hidden />
        </button>
        {gatewaySlot}
        <div className="composer-toolbar-spacer" />
        {counter && (
          <span id="sms-counter" aria-live="polite" aria-label="SMS character count">
            {counter}
          </span>
        )}
        <span id="shift-enter-hint" className="composer-hint" aria-hidden>
          {platform === "macos" ? "⇧↵ new line" : "Shift+Enter new line"}
        </span>
        <button
          className="send-button"
          aria-label="Send"
          aria-describedby={!sendSupported ? "unavailable-hint" : undefined}
          title={unavailableReason}
          onClick={onSend}
          disabled={!canSend}
        >
          <SendHorizontal size={18} aria-hidden />
        </button>
        </div>
      </div>
    </section>
  );
}

export function Panel({
  children,
  activeView,
  onView,
  connectionLabel,
  connectionState,
  onToggleList,
  listCollapsed,
  threadListId,
  notificationUnread = 0,
  contactsPending = 0,
}: {
  children?: ReactNode;
  activeView: "conversations" | "notifications" | "settings" | "contacts";
  onView(view: "conversations" | "notifications" | "settings" | "contacts"): void;
  connectionLabel: string;
  connectionState: string;
  onToggleList?(): void;
  listCollapsed?: boolean;
  threadListId?: string;
  notificationUnread?: number;
  contactsPending?: number;
}) {
  return (
    <aside id="desktop-rail" aria-label="Navigation rail">
      <span className="rail-mark" aria-hidden>
        ◈
      </span>
      <nav aria-label="Main navigation">
        <button
          data-rail-item="conversations"
          aria-label="Conversations"
          title="Conversations"
          aria-current={activeView === "conversations" ? "page" : undefined}
          aria-expanded={activeView === "conversations" ? !listCollapsed : undefined}
          aria-controls={activeView === "conversations" ? threadListId : undefined}
          onClick={() => activeView === "conversations" ? onToggleList?.() : onView("conversations")}
        >
          <MessageCircle size={20} aria-hidden />
        </button>
        <button data-rail-item="contacts" aria-label={contactsPending ? `Contacts, ${contactsPending} pending` : "Contacts"} title="Contacts" aria-current={activeView === "contacts" ? "page" : undefined} onClick={() => onView("contacts")}>
          <Users size={20} aria-hidden />
          {contactsPending > 0 && <span className="rail-notif-badge" aria-hidden>{contactsPending > 99 ? "99+" : contactsPending}</span>}
        </button>
        <button
          data-rail-item="notifications"
          aria-label={notificationUnread ? `Notifications, ${notificationUnread} unread` : "Notifications"}
          title="Notifications"
          aria-current={activeView === "notifications" ? "page" : undefined}
          onClick={() => onView("notifications")}
        >
          <Bell size={20} aria-hidden />
          {notificationUnread > 0 && <span className="rail-notif-badge" aria-hidden>{notificationUnread > 99 ? "99+" : notificationUnread}</span>}
        </button>
        <button
          data-rail-item="settings"
          aria-label="Settings"
          title="Settings"
          aria-current={activeView === "settings" ? "page" : undefined}
          onClick={() => onView("settings")}
        >
          <Settings size={20} aria-hidden />
        </button>
      </nav>
      <div className="rail-spacer" />
      <div
        role="status"
        aria-label={`Connection: ${connectionLabel}`}
        className="rail-connection-dot"
        data-connection-state={connectionState}
        title={`Connection: ${connectionLabel}`}
      >
        <span className="connection-dot" aria-hidden />
      </div>
      {children}
    </aside>
  );
}
