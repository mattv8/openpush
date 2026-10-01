import type { KeyboardEvent, ReactNode } from "react";
import { useEffect, useId, useRef, useState } from "react";
import {
  Check,
  Clock,
  Loader,
  MessageCircle,
  MessageSquarePlus,
  Minus,
  Paperclip,
  SendHorizontal,
  Settings,
  Square,
  X,
} from "lucide-react";
export { ResizeHandle, type ResizeHandleProps } from "./ResizeHandle";
export { installOverlayScrollbars, OVERLAY_SCROLL_HIDE_DELAY } from "./overlayScroll";
import "./styles.css";
export {
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
}: {
  onMinimize(): void;
  onMaximize(): void;
  onClose(): void;
  simulated?: boolean;
  platform?: "macos" | "windows" | "linux";
  isComposer?: boolean;
  title?: string;
}) {
  const macos = (platform ?? detectPlatform()) === "macos";
  return (
    <header
      id="desktop-titlebar"
      className="titlebar"
      data-tauri-drag-region
      role="banner"
      aria-label="OpenPush window controls"
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
        ) : "OpenPush"}
      </strong>
      <div className="titlebar-spacer" data-tauri-drag-region />
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
  loading = false,
}: {
  conversations: Conversation[];
  selectedId: string;
  onSelect(id: string): void;
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
              <button
                data-conversation-id={c.id}
                className={`conversation-row ${selectedId === c.id ? "selected" : ""}`}
                aria-current={selectedId === c.id ? "location" : undefined}
                onClick={() => onSelect(c.id)}
              >
                <span className="avatar" aria-hidden>
                  {c.name.slice(0, 1).toUpperCase()}
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
  | { kind: "new"; id: string; name: string; value: string };
const NEW_OPTION_ID = "new-recipient";
export function RecipientPicker({
  recipients,
  onChange,
  onNewRecipient,
}: {
  recipients: Conversation[];
  onChange(ids: string[]): void;
  onNewRecipient?(value: string): void;
}) {
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<string[]>([]);
  const [active, setActive] = useState(0);
  const listId = useId();
  const typed = query.trim();
  const options: PickerOption[] = [
    ...recipients
      .filter(
        (r) =>
          r.name.toLowerCase().includes(query.toLowerCase()) &&
          !chosen.includes(r.id),
      )
      .map((r) => ({ kind: "existing" as const, id: r.id, name: r.name })),
    ...(onNewRecipient && typed
      ? [
          {
            kind: "new" as const,
            id: NEW_OPTION_ID,
            name: `Message ${typed}`,
            value: typed,
          },
        ]
      : []),
  ];
  const reset = () => {
    setQuery("");
    setActive(0);
  };
  const commit = (option: PickerOption) => {
    if (option.kind === "new") {
      onNewRecipient?.(option.value);
      reset();
      return;
    }
    const next = [...chosen, option.id];
    setChosen(next);
    onChange(next);
    reset();
  };
  const keydown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing || !options.length) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((x) => Math.min(x + 1, options.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((x) => Math.max(x - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      commit(options[Math.min(active, options.length - 1)]);
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
        value={query}
        onKeyDown={keydown}
        onChange={(e) => {
          setQuery(e.target.value);
          setActive(0);
        }}
        placeholder="Search or start new"
      />
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
              onMouseDown={(e) => {
                e.preventDefault();
                commit(option);
              }}
            >
              {option.name}
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
  unavailableReason,
  gatewaySlot,
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
  unavailableReason?: string;
  gatewaySlot?: ReactNode;
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
      {status && (
        <p className="composer-status" role="status">
          {status}
        </p>
      )}
      {!sendSupported && !gatewaySlot && <span id="unavailable-hint">Sending unavailable: {unavailableReason ?? "gateway is offline"}</span>}
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
}: {
  children?: ReactNode;
  activeView: "conversations" | "settings";
  onView(view: "conversations" | "settings"): void;
  connectionLabel: string;
  connectionState: string;
  onToggleList?(): void;
  listCollapsed?: boolean;
  threadListId?: string;
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
