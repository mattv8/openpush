import type { KeyboardEvent, ReactNode } from "react";
import { useId, useState } from "react";
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

export function AppTitlebar({
  onMinimize,
  onMaximize,
  onClose,
  simulated = false,
}: {
  onMinimize(): void;
  onMaximize(): void;
  onClose(): void;
  simulated?: boolean;
}) {
  return (
    <header
      id="desktop-titlebar"
      className="titlebar"
      data-tauri-drag-region
      aria-label="OpenPush window controls"
    >
      <span className="titlebar-product" aria-hidden>
        ◈
      </span>
      <strong className="titlebar-wordmark" data-tauri-drag-region>
        OpenPush
      </strong>
      {simulated && (
        <span className="simulated" role="status">
          SIMULATED UI
        </span>
      )}
      <div className="titlebar-spacer" data-tauri-drag-region />
      <div
        id="window-controls"
        className="window-controls"
        role="toolbar"
        aria-label="Window controls"
      >
        <button
          aria-label="Minimize window"
          title="Minimize window"
          onClick={onMinimize}
        >
          <Minus size={14} aria-hidden />
        </button>
        <button
          aria-label="Maximize window"
          title="Maximize window"
          onClick={onMaximize}
        >
          <Square size={14} aria-hidden />
        </button>
        <button
          aria-label="Close window"
          title="Close window"
          onClick={onClose}
          className="close-button"
        >
          <X size={14} aria-hidden />
        </button>
      </div>
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
}: {
  draft: string;
  attachments: Attachment[];
  sendSupported: boolean;
  onDraftChange(value: string): void;
  onSend(): void;
  status?: string;
  onAddAttachment?(): void;
  unavailableReason?: string;
}) {
  const hasContent = Boolean(draft.trim()) || attachments.length > 0;
  const canSend = sendSupported && hasContent;
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
        <button
          type="button"
          aria-label="Add attachment"
          title="Add attachment"
          onClick={onAddAttachment}
          disabled={!onAddAttachment}
        >
          <Paperclip size={18} aria-hidden />
        </button>
        <label htmlFor="composer-textarea" className="sr-only">
          Message
        </label>
        <textarea
          id="composer-textarea"
          aria-label="Message"
          value={draft}
          onChange={(e) => onDraftChange(e.target.value)}
          onKeyDown={keydown}
          placeholder="Type a message"
          rows={1}
        />
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
      {!sendSupported && (
        <span id="unavailable-hint">
          Sending unavailable: {unavailableReason ?? "gateway is offline"}
        </span>
      )}
    </section>
  );
}

export function Panel({
  children,
  activeView,
  onView,
  connectionLabel,
  connectionState,
}: {
  children?: ReactNode;
  activeView: "conversations" | "settings";
  onView(view: "conversations" | "settings"): void;
  connectionLabel: string;
  connectionState: string;
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
          onClick={() => onView("conversations")}
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
