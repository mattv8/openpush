import type { KeyboardEvent, ReactNode } from "react";
import { useId, useState } from "react";
import "./styles.css";

export const tokens = { sidebarWidth: 280, rowHeight: 56, avatarSize: 40, messageFont: "15px", space: { xs: 4, sm: 8, md: 12, lg: 16, xl: 24 } } as const;
export const themeTokens = {
  light: { "--surface-0": "#f5f5f5", "--surface-1": "#fff", "--text": "#111", "--secondary": "#555", "--border": "#ddd", "--selection": "#e9edf5", "--status-good": "#087f23", "--status-bad": "#b3261e", "--focus": "#1958b7" },
  dark: { "--surface-0": "#1a1a1a", "--surface-1": "#242424", "--text": "#f0f0f0", "--secondary": "#adadad", "--border": "#444", "--selection": "#343c4a", "--status-good": "#6dce8c", "--status-bad": "#ffb4ab", "--focus": "#f8d875" },
} as const;

export type Conversation = { id: string; name: string; preview: string; unread: number; status?: string };
export type Attachment = { id: string; name: string; state: "pending" | "ready" | "uploading" | "failed"; error?: string; previewUrl?: string };

/** `simulated` shows the SIMULATED badge; only pass it for fixture/simulated sessions. */
export function AppTitlebar({ onMinimize, onMaximize, onClose, simulated = false }: { onMinimize(): void; onMaximize(): void; onClose(): void; simulated?: boolean }) {
  return (
    <header id="desktop-titlebar" className="titlebar" data-tauri-drag-region aria-label="OpenPush window controls">
      <strong>OpenPush</strong>
      {simulated && <span className="simulated">SIMULATED UI</span>}
      <div className="window-controls">
        <button aria-label="Minimize window" onClick={onMinimize}>−</button>
        <button aria-label="Maximize window" onClick={onMaximize}>□</button>
        <button aria-label="Close window" onClick={onClose}>×</button>
      </div>
    </header>
  );
}

export function ConversationList({ conversations, selectedId, onSelect, loading = false }: { conversations: Conversation[]; selectedId: string; onSelect(id: string): void; loading?: boolean }) {
  return (
    <nav id="conversation-list" aria-label="Conversations" aria-busy={loading}>
      <ul role="list">
        {loading ? <li className="sidebar-empty">Loading conversations…</li>
          : conversations.length ? conversations.map(c => (
            <li key={c.id}>
              <button data-conversation-id={c.id} className={`conversation-row ${selectedId === c.id ? "selected" : ""}`} aria-current={selectedId === c.id ? "location" : undefined} onClick={() => onSelect(c.id)}>
                <span className="avatar" aria-hidden>{c.name.slice(0, 1)}</span>
                <span className="conversation-copy"><b>{c.name}</b><small>{c.preview}</small></span>
                {c.unread > 0 && <span aria-label={`${c.unread} unread messages`} className="unread"><span aria-hidden>{c.unread}</span></span>}
              </button>
            </li>
          ))
          : <li className="sidebar-empty">No conversations yet</li>}
      </ul>
    </nav>
  );
}

type PickerOption = { kind: "existing"; id: string; name: string } | { kind: "new"; id: string; name: string; value: string };
const NEW_OPTION_ID = "new-recipient";

/**
 * Combobox over existing conversations. When `onNewRecipient` is provided, a final
 * "Message <typed value>" option starts a draft for an address that has no conversation yet.
 */
export function RecipientPicker({ recipients, onChange, onNewRecipient }: { recipients: Conversation[]; onChange(ids: string[]): void; onNewRecipient?(value: string): void }) {
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<string[]>([]);
  const [active, setActive] = useState(0);
  const listId = useId();
  const typed = query.trim();
  const options: PickerOption[] = [
    ...recipients.filter(r => r.name.toLowerCase().includes(query.toLowerCase()) && !chosen.includes(r.id)).map(r => ({ kind: "existing" as const, id: r.id, name: r.name })),
    ...(onNewRecipient && typed ? [{ kind: "new" as const, id: NEW_OPTION_ID, name: `Message ${typed}`, value: typed }] : []),
  ];
  const reset = () => { setQuery(""); setActive(0); };
  const commit = (option: PickerOption) => {
    if (option.kind === "new") { onNewRecipient?.(option.value); reset(); return; }
    const next = [...chosen, option.id];
    setChosen(next); onChange(next); reset();
  };
  const remove = (id: string) => { const next = chosen.filter(x => x !== id); setChosen(next); onChange(next); };
  const keydown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing || !options.length) return;
    if (event.key === "ArrowDown") { event.preventDefault(); setActive(x => Math.min(x + 1, options.length - 1)); }
    else if (event.key === "ArrowUp") { event.preventDefault(); setActive(x => Math.max(x - 1, 0)); }
    else if (event.key === "Enter") { event.preventDefault(); commit(options[Math.min(active, options.length - 1)]); }
    else if (event.key === "Escape") setQuery("");
  };
  const activeOption = options[active];
  return (
    <section id="recipient-picker" aria-label="New message recipients">
      <h2 className="sidebar-section-heading">New message</h2>
      <div className="chips">
        {chosen.map(id => {
          const r = recipients.find(x => x.id === id);
          return <button key={id} className="chip" data-recipient-id={id} onClick={() => remove(id)} aria-label={`Remove ${r?.name ?? id}`}>{r?.name ?? id} ×</button>;
        })}
      </div>
      <input id="recipient-search" role="combobox" aria-autocomplete="list" aria-expanded={Boolean(query) && options.length > 0} aria-haspopup="listbox" aria-controls={listId}
        aria-activedescendant={query && activeOption ? `${listId}-${activeOption.id}` : undefined} aria-label="Search recipients" value={query} onKeyDown={keydown}
        onChange={e => { setQuery(e.target.value); setActive(0); }} placeholder={onNewRecipient ? "Name or phone number" : "Search recipients"} />
      {query && (
        <ul id={listId} role="listbox">
          {options.map((option, index) => (
            <li id={`${listId}-${option.id}`} role="option" aria-selected={index === active} key={option.id} data-recipient-id={option.kind === "existing" ? option.id : undefined} data-new-recipient={option.kind === "new" ? "true" : undefined}
              onMouseDown={e => { e.preventDefault(); commit(option); }}>{option.name}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function Composer({ draft, attachments, sendSupported, onDraftChange, onSend, status, onAddAttachment, unavailableReason }: { draft: string; attachments: Attachment[]; sendSupported: boolean; onDraftChange(value: string): void; onSend(): void; status?: string; onAddAttachment?(): void; unavailableReason?: string }) {
  const hasContent = Boolean(draft.trim()) || attachments.length > 0;
  const canSend = sendSupported && hasContent;
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      if (canSend) onSend();
    }
  };
  const labels = { pending: "Pending", ready: "Ready", uploading: "Uploading…", failed: "Failed" };
  return (
    <section id="shared-composer" className="composer" aria-label="Message composer">
      <ul id="attachment-tray" aria-label="Attachments">
        {attachments.map(a => (
          <li key={a.id} data-attachment-id={a.id} className={`attachment ${a.state}`}>
            {a.previewUrl && <img src={a.previewUrl} alt={a.name} onError={e => (e.currentTarget.style.display = "none")} />}
            {a.name} · {labels[a.state]}{a.error && `: ${a.error}`}
          </li>
        ))}
      </ul>
      <label htmlFor="composer-textarea" className="sr-only">Message</label>
      <textarea id="composer-textarea" aria-label="Message" value={draft} onChange={e => onDraftChange(e.target.value)} onKeyDown={onKeyDown} placeholder="Write a message" />
      <div className="composer-actions">
        <button type="button" onClick={onAddAttachment} disabled={!onAddAttachment}>Add attachment</button>
        <span className="keyboard-hint" aria-hidden>{sendSupported ? "Enter to send · Shift+Enter for new line" : `Sending unavailable: ${unavailableReason ?? "gateway is offline"}`}</span>
        <button onClick={onSend} disabled={!canSend}>Send</button>
      </div>
      {status && <p className="composer-status" role="status">{status}</p>}
    </section>
  );
}

export function GatewayHealth({ online, name }: { online: boolean; name: string }) {
  return <section id="gateway-health" aria-label="Gateway health"><b>{name}</b><span className={online ? "status good" : "status bad"}>{online ? "Connected" : "Offline"}</span></section>;
}

export function SettingsPanel({ theme, onTheme, headStatus }: { theme: "light" | "dark" | "system"; onTheme(v: "light" | "dark" | "system"): void; headStatus: string }) {
  return (
    <section id="settings-panel" aria-label="Settings">
      <label>Theme <select value={theme} onChange={e => onTheme(e.target.value as "light" | "dark" | "system")}><option>system</option><option>light</option><option>dark</option></select></label>
      <p role="status">Floating heads: {headStatus}</p>
    </section>
  );
}

export function Panel({ children }: { children: ReactNode }) {
  return <aside id="desktop-sidebar" className="sidebar" aria-label="Sidebar">{children}</aside>;
}
