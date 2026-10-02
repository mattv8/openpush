import {
  useEffect,
  useLayoutEffect,
  useReducer,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type RefObject,
} from "react";
import {
  AppTitlebar,
  Check,
  CheckCheck,
  CheckCircle,
  Clock,
  Composer,
  ConversationList,
  ExternalLink,
  FileText,
  Image,
  Lock,
  LockKeyhole,
  LockOpen,
  MessageSquare,
  MessageSquarePlus,
  Panel,
  Radio,
  RecipientPicker,
  ResizeHandle,
  ShieldAlert,
  SquarePen,
  TriangleAlert,
  X,
  type Attachment,
  type Conversation,
  detectPlatform,
  installOverlayScrollbars,
} from "@openpush/desktop-ui";
import {
  bridge,
  type AttachmentView,
  type ConversationView,
  type DesktopSnapshot,
  type Draft,
  type DraftInput,
  type GatewayView,
  type MessageStatus,
  type AppFilter,
  type NotificationPreferences,
  type PublicCopy,
} from "./bridge";
import { NotificationsView } from "./Notifications";
import { NotificationSettings } from "./NotificationSettings";

/* ------------------------------------------------------------------ labels and errors */

const STATUS_LABEL: Record<MessageStatus, string> = {
  "queued-local": "Queued locally",
  "server-accepted": "Server accepted",
  "gateway-persisted": "Gateway persisted",
  preparing: "Preparing",
  submitted: "Submitted",
  sent: "Sent",
  "delivery-confirmed": "Delivery confirmed",
  "failed-before-submit": "Failed before submit",
  "failed-confirmed": "Failed",
  unknown: "Unknown delivery state — not retried",
};

const CONNECTION_STATE_LABEL: Record<
  DesktopSnapshot["connection"]["state"],
  string
> = {
  connected: "Connected",
  offline: "Offline",
  "missing-native-host": "Native host unavailable",
  error: "Connection error",
};

/** Known native connection codes; unknown codes are shown verbatim (they are static identifiers). */
const CONNECTION_CODE_LABEL: Record<string, string> = {
  "server-required": "configure a server URL",
  "credentials-required": "import device credentials",
  connecting: "connecting…",
  revoked: "this device was revoked; local data is kept",
  "outbox-rejected":
    "the server rejected queued messages; they stay queued locally",
  "live-timeout": "live connection timed out; reconnecting",
};

const FALLBACK_ERROR =
  "The native operation failed. Your edits remain in this window.";
const COMPOSER_MIN_HEIGHT = 96;
const COMPOSER_ATTACHMENT_MIN_HEIGHT = 132;
const RAIL_WIDTH = 48;
const LIST_MIN_WIDTH = 200;
const LIST_MAX_WIDTH = 480;
const PANE_MIN_WIDTH = 360;
const LIST_COLLAPSE_THRESHOLD = 120;
const TEXTAREA_LINE_HEIGHT = 22;
const TEXTAREA_MAX_HEIGHT = 176;

export const errorText = (error: unknown): string =>
  typeof error === "object" &&
  error !== null &&
  "message" in error &&
  typeof error.message === "string" &&
  error.message
    ? error.message
    : FALLBACK_ERROR;

const errorCode = (error: unknown): string | undefined =>
  typeof error === "object" &&
  error !== null &&
  "code" in error &&
  typeof error.code === "string"
    ? error.code
    : undefined;

function connectionText(connection: DesktopSnapshot["connection"]): string {
  const state = CONNECTION_STATE_LABEL[connection.state];
  if (!connection.errorCode || connection.errorCode === connection.state) return state;
  return `${state} — ${CONNECTION_CODE_LABEL[connection.errorCode] ?? connection.errorCode}`;
}

function expiryText(seconds: number): string {
  if (seconds <= 0)
    return "No expiry was reported; revoke it on the server when it is no longer needed.";
  if (seconds < 3600) return `Expires in ${Math.ceil(seconds / 60)} min.`;
  if (seconds < 172800) return `Expires in ${Math.round(seconds / 3600)} h.`;
  return `Expires in ${Math.round(seconds / 86400)} days.`;
}

function fileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function attachmentState(file: AttachmentView): string {
  if (file.state === "failed")
    return `Failed${file.error ? `: ${file.error}` : ""}`;
  return file.state === "uploading"
    ? "Uploading…"
    : file.state.charAt(0).toUpperCase() + file.state.slice(1);
}

/* ------------------------------------------------------------------ window context */

/** Composer windows are opened natively as `index.html?window=composer&conversationId=<uuid>`. */
export function readComposerConversation(search: string): string | null {
  const params = new URLSearchParams(search);
  const id = params.get("conversationId");
  return params.get("window") === "composer" &&
    id &&
    /^[A-Za-z0-9_-]{1,128}$/.test(id)
    ? id
    : null;
}

/* ------------------------------------------------------------------ gateway routes */

type DraftContent = Pick<
  Draft,
  "text" | "recipientIds" | "attachmentIds" | "gatewayId" | "simId"
>;

/** Gateway and SIM together identify a route; one device can expose several SIMs. */
const routeKey = (gatewayId: string, simId: string) =>
  `${encodeURIComponent(gatewayId)} ${encodeURIComponent(simId)}`;

type RouteChoice =
  | { kind: "route"; gateway: GatewayView; implicit: boolean }
  | { kind: "stale"; gatewayId: string; simId: string }
  | { kind: "none" };

/**
 * The stored route must match a reported gateway+SIM exactly; a vanished route is reported as
 * stale and never replaced. Only a draft without any stored route on a single-route setup uses
 * that one route implicitly.
 */
function resolveRoute(
  content: DraftContent,
  gateways: GatewayView[],
): RouteChoice {
  if (content.gatewayId && content.simId) {
    const gateway = gateways.find(
      (item) => item.id === content.gatewayId && item.simId === content.simId,
    );
    return gateway
      ? { kind: "route", gateway, implicit: false }
      : { kind: "stale", gatewayId: content.gatewayId, simId: content.simId };
  }
  return gateways.length === 1
    ? { kind: "route", gateway: gateways[0], implicit: true }
    : { kind: "none" };
}

function routeProblem(
  route: RouteChoice,
  gateways: GatewayView[],
  requiresMms: boolean,
  recipientCount: number,
  estimatedBytes: number,
): string | undefined {
  if (route.kind === "none")
    return gateways.length
      ? "choose a gateway and SIM"
      : "no gateway is configured";
  if (route.kind === "stale")
    return "the selected gateway/SIM is no longer reported; choose another route";
  if (!requiresMms && !route.gateway.supportsSms)
    return "this gateway/SIM does not support SMS";
  if (requiresMms && !route.gateway.supportsMms)
    return "this gateway/SIM does not support MMS attachments";
  const contentVersion = route.gateway.mmsContentVersion;
  if (requiresMms && (!Number.isSafeInteger(contentVersion) || (contentVersion ?? 0) < 2))
    return "this gateway/SIM needs MMS content version 2";
  const recipientLimit = Math.min(route.gateway.mmsMaxRecipients ?? 20, 20);
  if (requiresMms && recipientCount > recipientLimit)
    return `this MMS route supports up to ${recipientLimit} recipients`;
  if (requiresMms && route.gateway.mmsMaxBytes !== undefined && estimatedBytes > route.gateway.mmsMaxBytes)
    return `the MMS estimate exceeds the ${fileSize(route.gateway.mmsMaxBytes)} limit`;
  return undefined;
}

const requiresMms = (content: DraftContent, conversation?: ConversationView) =>
  content.recipientIds.length > 1 ||
  content.attachmentIds.length > 0 ||
  conversation?.messages.at(-1)?.transport === "mms";
/** Native participants are reply addressees and exclude the confirmed self address. */
const recipientCount = (content: DraftContent, conversation?: ConversationView) =>
  content.recipientIds.length || conversation?.participants?.length || 1;
const draftEstimateBytes = (content: DraftContent, attachments: Record<string, AttachmentView>) =>
  new TextEncoder().encode(content.text).length + content.attachmentIds.reduce((total, id) => total + (attachments[id]?.byteSize ?? 0), 0);

/* ------------------------------------------------------------------ draft store */

/** Local key for a new-recipient draft that has no native draft/conversation ID yet. */
const NEW_DRAFT_PREFIX = "new:";
const isLocalDraftKey = (key: string) => key.startsWith(NEW_DRAFT_PREFIX);

export type DraftSlot = {
  /** Conversation ID once known, otherwise a `new:` local key. */
  key: string;
  /** Native identity; empty strings ask the host to create the draft (and conversation). */
  id: string;
  conversationId: string;
  /** Last revision acknowledged by the host; used as the CAS `expectedRevision`. */
  revision: string;
  content: DraftContent;
  /** Local edit counter, and the newest edit the host has acknowledged. */
  generation: number;
  savedGeneration: number;
  error?: { message: string; code?: string };
  /** While a send is in flight, saves are deferred so they cannot race the atomic send/clear. */
  held: boolean;
  queue: Promise<unknown>;
};

export type SaveOutcome = { ok: true } | { ok: false; error: string };

const blankContent = (): DraftContent => ({
  text: "",
  recipientIds: [],
  attachmentIds: [],
});
const contentOf = (draft: Draft): DraftContent => ({
  text: draft.text,
  recipientIds: draft.recipientIds,
  attachmentIds: draft.attachmentIds,
  gatewayId: draft.gatewayId,
  simId: draft.simId,
});
const isDirty = (slot: DraftSlot) => slot.generation > slot.savedGeneration;

type DraftStoreEvents = {
  changed(): void;
  rekeyed(from: string, to: string): void;
};

/**
 * Per-conversation draft state with serialized CAS saves. A failed save keeps the local edits
 * dirty with an error; callers that must not lose content (send, close) flush and check the
 * outcome. Host snapshots never overwrite dirty or held drafts.
 */
export class DraftStore {
  private slots = new Map<string, DraftSlot>();
  private aliases = new Map<string, string>();
  private newCount = 0;

  constructor(
    private readonly persist: (input: DraftInput) => Promise<Draft>,
    private readonly events: DraftStoreEvents,
  ) {}

  /** Follows `new:` keys to the conversation ID the host assigned. */
  resolve(key: string): string {
    let current = key;
    for (
      let next = this.aliases.get(current);
      next !== undefined;
      next = this.aliases.get(current)
    )
      current = next;
    return current;
  }

  get(key: string): DraftSlot | undefined {
    return this.slots.get(this.resolve(key));
  }

  localDrafts(): DraftSlot[] {
    return [...this.slots.values()].filter((slot) => isLocalDraftKey(slot.key));
  }

  /** Starts a draft for a recipient without a conversation; the host assigns both IDs on save. */
  createNew(recipient: string): { key: string; saved: Promise<SaveOutcome> } {
    const key = `${NEW_DRAFT_PREFIX}${++this.newCount}`;
    return { key, saved: this.edit(key, { recipientIds: [recipient] }) };
  }

  edit(key: string, patch: Partial<DraftContent>): Promise<SaveOutcome> {
    const slot = this.ensure(key);
    slot.content = { ...slot.content, ...patch };
    slot.generation += 1;
    this.events.changed();
    return this.enqueue(slot);
  }

  /** Applies the host's view of a conversation's draft unless local edits are pending. */
  adopt(conversationId: string, draft: Draft | undefined) {
    const slot = this.get(conversationId);
    if (slot && (isDirty(slot) || slot.held)) return;
    if (draft) {
      const target = slot ?? this.ensure(conversationId);
      Object.assign(target, {
        id: draft.id,
        conversationId: draft.conversationId,
        revision: draft.revision,
        content: contentOf(draft),
        error: undefined,
      });
    } else if (slot) {
      Object.assign(slot, {
        id: "",
        revision: "0",
        content: blankContent(),
        error: undefined,
      });
    } else return;
    this.events.changed();
  }

  /** Waits for queued saves and retries once if edits are still unacknowledged. */
  async flush(key: string): Promise<SaveOutcome> {
    const slot = this.get(key);
    if (!slot) return { ok: true };
    await slot.queue;
    return isDirty(slot) ? this.enqueue(slot) : { ok: true };
  }

  /** After a stale-revision conflict the person may keep this window's text over the stored one. */
  rebase(key: string, stored: Draft | undefined) {
    const slot = this.get(key);
    if (!slot) return;
    if (stored)
      Object.assign(slot, { id: stored.id, revision: stored.revision });
    else Object.assign(slot, { id: "", revision: "0" });
  }

  hold(key: string) {
    const slot = this.get(key);
    if (slot) slot.held = true;
  }

  release(key: string) {
    const slot = this.get(key);
    if (!slot) return;
    slot.held = false;
    if (isDirty(slot)) void this.enqueue(slot);
  }

  /** The host cleared the sent draft; edits typed during the send start a fresh draft. */
  markSent(key: string, sentGeneration: number) {
    const slot = this.get(key);
    if (!slot) return;
    Object.assign(slot, {
      id: "",
      revision: "0",
      held: false,
      error: undefined,
    });
    if (slot.generation > sentGeneration) {
      void this.enqueue(slot);
    } else {
      slot.content = blankContent();
      slot.savedGeneration = slot.generation;
    }
    this.events.changed();
  }

  private ensure(key: string): DraftSlot {
    const resolved = this.resolve(key);
    let slot = this.slots.get(resolved);
    if (!slot) {
      slot = {
        key: resolved,
        id: "",
        conversationId: isLocalDraftKey(resolved) ? "" : resolved,
        revision: "0",
        content: blankContent(),
        generation: 0,
        savedGeneration: 0,
        held: false,
        queue: Promise.resolve(),
      };
      this.slots.set(resolved, slot);
    }
    return slot;
  }

  private enqueue(slot: DraftSlot): Promise<SaveOutcome> {
    const run = slot.queue.then(() => this.save(slot));
    slot.queue = run;
    return run;
  }

  private async save(slot: DraftSlot): Promise<SaveOutcome> {
    if (slot.held)
      return {
        ok: false,
        error:
          "A send is in progress; your edits are kept and saved afterwards.",
      };
    if (!isDirty(slot))
      return slot.error
        ? { ok: false, error: slot.error.message }
        : { ok: true };
    const generation = slot.generation;
    const input: DraftInput = {
      id: slot.id,
      conversationId: slot.conversationId,
      ...slot.content,
      expectedRevision: slot.revision,
    };
    let saved: Draft;
    try {
      saved = await this.persist(input);
    } catch (error) {
      slot.error = { message: errorText(error), code: errorCode(error) };
      this.events.changed();
      return { ok: false, error: slot.error.message };
    }
    Object.assign(slot, {
      id: saved.id,
      revision: saved.revision,
      savedGeneration: Math.max(slot.savedGeneration, generation),
      error: undefined,
    });
    if (slot.generation === generation) slot.content = contentOf(saved);
    if (saved.conversationId && saved.conversationId !== slot.conversationId)
      this.rekey(slot, saved.conversationId);
    this.events.changed();
    return { ok: true };
  }

  private rekey(slot: DraftSlot, conversationId: string) {
    const from = slot.key;
    slot.conversationId = conversationId;
    if (from === conversationId) return;
    this.slots.delete(from);
    this.aliases.set(from, conversationId);
    slot.key = conversationId;
    this.slots.set(conversationId, slot);
    this.events.rekeyed(from, conversationId);
  }
}

/* ------------------------------------------------------------------ presentational pieces */

type Theme = "light" | "dark" | "system";

function GatewaySelector({
  gateways,
  content,
  problem,
  mmsRequired,
  estimatedBytes,
  onSelect,
}: {
  gateways: GatewayView[];
  content: DraftContent;
  problem?: string;
  mmsRequired: boolean;
  estimatedBytes: number;
  onSelect(gateway: GatewayView): void;
}) {
  const route = resolveRoute(content, gateways);
  const value =
    route.kind === "route"
      ? routeKey(route.gateway.id, route.gateway.simId)
      : route.kind === "stale"
        ? routeKey(route.gatewayId, route.simId)
        : "";
  const choose = (key: string) => {
    const gateway = gateways.find(
      (item) => routeKey(item.id, item.simId) === key,
    );
    if (gateway) onSelect(gateway);
  };
  const simLabel = (gateway: GatewayView) => {
    const index = gateways.findIndex((item) => item.id === gateway.id && item.simId === gateway.simId);
    return index >= 0 ? `SIM ${index + 1}` : gateway.simId;
  };
  return (
    <section id="gateway-selector" aria-label="Gateway and SIM" data-route-state={problem ? "blocked" : "ready"}>
        <select
          aria-label="Gateway"
          value={value}
          onChange={(event) => choose(event.target.value)}
          disabled={!gateways.length}
        >
          {route.kind === "none" && (
            <option value="" disabled>
              {gateways.length ? "Choose gateway and SIM" : "No gateway"}
            </option>
          )}
          {route.kind === "stale" && (
            <option value={value} disabled>
              Unavailable route · SIM {route.simId}
            </option>
          )}
          {gateways.map((item) => {
            const key = routeKey(item.id, item.simId);
            return (
              <option
                key={key}
                value={key}
                data-gateway-id={item.id}
                data-sim-id={item.simId}
              >
                {item.name} · {simLabel(item)}{item.simulated ? " · Simulated" : ""}
              </option>
            );
          })}
        </select>
      <span className="route-status-icon" aria-hidden>
        {problem ? (
          <TriangleAlert size={14} aria-hidden />
        ) : (
          <Radio size={14} aria-hidden />
        )}
      </span>
      <span id="unavailable-hint" hidden={!problem}>Sending unavailable: {problem}</span>
      {mmsRequired && route.kind === "route" && estimatedBytes > 0 && (
        <span className="mms-limit" data-mms-limit-source={route.gateway.mmsLimitSource ?? "fallback"}>
          {fileSize(estimatedBytes)} used · {fileSize(route.gateway.mmsMaxBytes ?? 300 * 1024)} limit ({route.gateway.mmsLimitSource === "carrier" ? "carrier" : "fallback"}) · up to {Math.min(route.gateway.mmsMaxRecipients ?? 20, 20)} recipients
        </span>
      )}
    </section>
  );
}

/** Recipients of a new conversation; committed on blur/Enter so partial numbers are not saved. */
function RecipientField({
  recipients,
  onCommit,
}: {
  recipients: string[];
  onCommit(ids: string[]): void;
}) {
  const joined = recipients.join(", ");
  const [text, setText] = useState(joined);
  const focused = useRef(false);
  useEffect(() => {
    if (!focused.current) setText(joined);
  }, [joined]);
  const commit = () => {
    const ids = text
      .split(/[,;]/)
      .map((value) => value.trim())
      .filter(Boolean);
    if (ids.join(", ") !== joined) onCommit(ids);
  };
  const keydown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      commit();
    }
  };
  return (
    <section id="draft-recipients" aria-label="Message recipients">
      <label>
        To{" "}
        <input
          aria-label="Recipients"
          aria-describedby="draft-recipients-hint"
          value={text}
          placeholder="+1 555 0100"
          onChange={(event) => setText(event.target.value)}
          onFocus={() => {
            focused.current = true;
          }}
          onBlur={() => {
            focused.current = false;
            commit();
          }}
          onKeyDown={keydown}
        />
      </label>
      <small id="draft-recipients-hint">
        Separate numbers with commas. SMS goes to one recipient; group messages
        need an MMS-capable route.
      </small>
    </section>
  );
}

function DraftRecovery({ error, onRetry }: { error: string; onRetry(): void }) {
  return (
    <section id="draft-recovery" role="alert" aria-label="Unsaved draft">
      <p>
        Draft not saved: {error} Your text is still here; sending and closing
        stay blocked until it is saved.
      </p>
      <button onClick={onRetry}>Retry save</button>
    </section>
  );
}

type RowRegistry = (id: string, row: HTMLElement | null) => void;

function MessageList({
  conversation,
  loading,
  registerRow,
  publicCopies,
  onPublish,
  onRetryAttachment,
  onSaveAttachment,
  listRef,
  onScroll,
}: {
  conversation?: ConversationView;
  loading: boolean;
  registerRow: RowRegistry;
  publicCopies: Record<string, PublicCopy>;
  onPublish(attachment: AttachmentView): void;
  onRetryAttachment(attachment: AttachmentView): void;
  onSaveAttachment(attachment: AttachmentView): void;
  listRef: RefObject<HTMLDivElement | null>;
  onScroll(): void;
}) {
  if (loading)
    return (
      <div
        ref={listRef}
        id="message-list"
        role="log"
        aria-label="Messages"
        aria-busy="true"
        onScroll={onScroll}
      >
        <p>Loading conversation…</p>
      </div>
    );
  if (!conversation)
    return (
      <div
        ref={listRef}
        id="message-list"
        role="log"
        aria-label="Messages"
        onScroll={onScroll}
      >
        <div id="no-selection-state">
          <MessageSquare size={32} aria-hidden />
          <p>Select a conversation or start a new message</p>
        </div>
      </div>
    );
  return (
    <div
      ref={listRef}
      id="message-list"
      role="log"
      aria-label="Messages"
      onScroll={onScroll}
    >
      <div id="message-list-content">
        {!conversation.messages.length && <p>Start the conversation.</p>}
        {conversation.messages.map((message) => (
          <article
            ref={(row) => registerRow(message.id, row)}
            key={message.id}
            data-message-id={message.id}
            className={`message-bubble ${message.sender}`}
          >
            <p>{message.body}</p>
            {(message.transport || message.subject || message.participants?.length) && (
              <p
                className="message-mms-details"
                data-message-transport={message.transport}
                aria-label={[message.transport && `Transport: ${message.transport.toUpperCase()}`, message.subject && `Subject: ${message.subject}`, message.participants?.length && `Participants: ${message.participants.join(", ")}`].filter(Boolean).join(". ")}
              >
                {message.transport && `Transport: ${message.transport.toUpperCase()}`}
                {message.subject && `${message.transport ? " · " : ""}Subject: ${message.subject}`}
                {message.participants?.length ? `${message.transport || message.subject ? " · " : ""}Participants: ${message.participants.join(", ")}` : ""}
              </p>
            )}
            {message.attachments.map((file) => {
              const copy = publicCopies[file.id];
              const shareable =
                file.state === "ready" && file.mediaType.startsWith("image/");
              return (
                <div
                  key={file.id}
                  data-attachment-id={file.id}
                  className="file-card"
                >
                  {file.previewUrl && (
                    <img
                      src={file.previewUrl}
                      alt={file.name}
                      onError={(event) => {
                        event.currentTarget.hidden = true;
                      }}
                    />
                  )}
                  {file.mediaType.startsWith("image/") ? (
                    <Image size={16} aria-hidden />
                  ) : (
                    <FileText size={16} aria-hidden />
                  )}
                  <span>{file.name}</span>
                  <small>
                    {fileSize(file.byteSize)} · {attachmentState(file)}
                  </small>
                  {file.retryable && file.transfer && (
                    <button className="secondary-button" onClick={() => onRetryAttachment(file)} aria-label={`Retry ${file.transfer} for ${file.name}`}>
                      Retry {file.transfer}
                    </button>
                  )}
                  {file.state === "ready" && (
                    <button className="secondary-button" onClick={() => onSaveAttachment(file)} aria-label={`Save ${file.name}`}>
                      Save {file.name}
                    </button>
                  )}
                  {shareable && !copy && (
                    <button
                      className="secondary-button"
                      onClick={() => onPublish(file)}
                      aria-label={`Create public link for ${file.name}`}
                    >
                      Create public link
                    </button>
                  )}
                  {copy && (
                    <div className="public-copy" data-public-copy-for={file.id}>
                      <label>
                        Public link{" "}
                        <span className="public-link-warning" role="status">
                          <ShieldAlert size={12} aria-hidden /> Anyone with this
                          link can view this copy
                        </span>
                        <input
                          readOnly
                          aria-label={`Public link for ${file.name}`}
                          value={copy.url}
                          onFocus={(event) => event.currentTarget.select()}
                        />
                      </label>
                      <small>{expiryText(copy.expiresInSeconds)}</small>
                    </div>
                  )}
                </div>
              );
            })}
            <footer className="bubble-footer">
              <time>{message.timestamp}</time>
              {message.status && (
                <span data-status={message.status}>
                  {["failed-before-submit", "failed-confirmed"].includes(
                    message.status,
                  ) ? (
                    <X size={10} aria-hidden />
                  ) : message.status === "unknown" ? (
                    <TriangleAlert size={10} aria-hidden />
                  ) : message.status === "delivery-confirmed" ? (
                    <CheckCheck size={10} aria-hidden />
                  ) : message.status === "sent" ? (
                    <Check size={10} aria-hidden />
                  ) : (
                    <Clock size={10} aria-hidden />
                  )}{" "}
                  {STATUS_LABEL[message.status]}
                </span>
              )}
            </footer>
          </article>
        ))}
      </div>
    </div>
  );
}

function OnboardingView({
  connected,
  canUnlock,
  origin,
  onOrigin,
  onAction,
  encryption,
}: {
  connected: boolean;
  canUnlock: boolean;
  origin: string;
  onOrigin(value: string): void;
  onAction(action: "origin" | "credentials" | "unlock"): void;
  encryption: DesktopSnapshot["encryption"]["state"];
}) {
  const step = (
    number: string,
    title: string,
    body: React.ReactNode,
    done: boolean,
    active: boolean,
  ) => (
    <li
      data-step={number}
      data-step-state={done ? "done" : active ? "active" : "pending"}
    >
      <span className="step-number" aria-hidden>
        {done ? <CheckCircle /> : number}
      </span>
      <div className="step-body">
        <strong>{title}</strong>
        {body}
      </div>
    </li>
  );
  return (
    <section id="onboarding-view" aria-label="Set up OpenPush" role="region">
      <header id="onboarding-header">
        <h1>Set up OpenPush</h1>
        <p id="onboarding-subtitle">
          {connected ? "Connected" : "Connection setup is needed"}
        </p>
      </header>
      <ol id="onboarding-steps" role="list">
        {step(
          "1",
          "Configure server",
          <>
            <p>Enter your OpenPush server URL and apply it.</p>
            <label>
              Server URL{" "}
              <input
                id="onboarding-server-url"
                aria-label="Server URL"
                value={origin}
                onChange={(event) => onOrigin(event.target.value)}
                placeholder="https://server.example"
              />
            </label>
            <button
              className="primary-button"
              data-action="configure-server"
              onClick={() => onAction("origin")}
            >
              Configure server
            </button>
          </>,
          Boolean(origin),
          !origin,
        )}
        {step(
          "2",
          "Import device credentials",
          <>
            <p>
              Credentials are handled entirely by the native host — this app
              never receives them.
            </p>
            <button
              className="secondary-button"
              data-action="import-credentials"
              onClick={() => onAction("credentials")}
            >
              Import credentials natively
            </button>
          </>,
          false,
          Boolean(origin),
        )}
        {encryption !== "unlocked" &&
          step(
            "3",
            "Unlock sync encryption",
            <>
              <p>
                Your passphrase is handled natively. Required before messages
                can sync.
              </p>
              <button
                className="secondary-button"
                data-action="unlock-sync"
                disabled={!canUnlock}
                onClick={() => onAction("unlock")}
              >
                Unlock sync natively
              </button>
            </>,
            false,
            canUnlock,
          )}
      </ol>
    </section>
  );
}

function SettingsView({
  origin,
  onOrigin,
  onAction,
  encryption,
  canUnlock,
  theme,
  onTheme,
  headStatus,
  notifications,
  filters,
  sources,
  preferences,
  onPreferences,
  onMute,
  onPermission,
}: {
  origin: string;
  onOrigin(value: string): void;
  onAction(action: "origin" | "credentials" | "unlock"): void;
  encryption: DesktopSnapshot["encryption"]["state"];
  canUnlock: boolean;
  theme: Theme;
  onTheme(theme: Theme): void;
  headStatus: string;
  notifications: DesktopSnapshot["notifications"];
  filters: AppFilter[];
  sources: { id: string; name: string }[];
  preferences: NotificationPreferences;
  onPreferences(preferences: NotificationPreferences): Promise<void>;
  onMute(filter: AppFilter): Promise<void>;
  onPermission(): Promise<void>;
}) {
  const syncText =
    encryption === "unlocked"
      ? "Device sync encrypted"
      : encryption === "mismatch"
        ? "Device sync key mismatch"
        : "Device sync not unlocked";

  return (
    <section id="settings-view" aria-label="Settings" role="region">
      <section data-settings-section="server">
        <h2>Server</h2>
        <p>Choose the OpenPush server this desktop app connects to.</p>
        <div className="settings-control-row">
          <label>
            Server URL
            <input
              aria-label="Server URL"
              value={origin}
              onChange={(event) => onOrigin(event.target.value)}
              placeholder="https://server.example"
            />
          </label>
          <button className="primary-button" onClick={() => onAction("origin")}>
            Configure server
          </button>
        </div>
      </section>
      <section data-settings-section="credentials">
        <h2>Device credentials</h2>
        <p>Credentials are imported natively and are never shown here.</p>
        <button
          className="secondary-button"
          onClick={() => onAction("credentials")}
        >
          Import credentials natively
        </button>
      </section>
      <section data-settings-section="sync">
        <h2>Sync encryption</h2>
        <p className="settings-sync-state">
          {encryption === "unlocked" ? (
            <LockOpen size={14} aria-hidden />
          ) : (
            <Lock size={14} aria-hidden />
          )}
          {syncText}
        </p>
        {canUnlock && (
          <button
            className="secondary-button"
            onClick={() => onAction("unlock")}
          >
            Unlock sync natively
          </button>
        )}
      </section>
      <NotificationSettings
        notifications={notifications}
        filters={filters}
        sources={sources}
        preferences={preferences}
        onPreferences={onPreferences}
        onMute={onMute}
        onPermission={onPermission}
      />
      <section data-settings-section="theme">
        <h2>Appearance</h2>
        <p>Choose how OpenPush follows your system appearance.</p>
        <div className="settings-control-row settings-theme-row">
          <label>
            Theme
            <select
              value={theme}
              onChange={(event) => onTheme(event.target.value as Theme)}
            >
              <option>system</option>
              <option>light</option>
              <option>dark</option>
            </select>
          </label>
        </div>
      </section>
      <section data-settings-section="heads">
        <h2>Conversation heads</h2>
        <p role="status">Floating heads: {headStatus}</p>
      </section>
    </section>
  );
}

function TitlebarStatus({
  connection,
  encryption,
}: {
  connection: DesktopSnapshot["connection"];
  encryption: DesktopSnapshot["encryption"]["state"];
}) {
  const [icon, text] =
    encryption === "unlocked"
      ? [<LockOpen size={12} aria-hidden />, "Device sync encrypted"]
      : encryption === "mismatch"
        ? [<LockKeyhole size={12} aria-hidden />, "Device sync key mismatch"]
        : [<Lock size={12} aria-hidden />, "Device sync not unlocked"];
  const label = connectionText(connection);
  return (
    <>
      <span className="titlebar-pill" data-disclosure="sync-state" data-sync-state={encryption} role="status" aria-label={text} title={text}>
        {icon}<span className="titlebar-pill-label">{text}</span>
      </span>
      <span className="titlebar-pill" data-disclosure="carrier-sms" role="status" aria-label="Carrier SMS/MMS not end-to-end encrypted" title="Carrier SMS/MMS not end-to-end encrypted">
        <ShieldAlert size={12} aria-hidden />
        <span className="titlebar-pill-label">Carrier SMS/MMS not end-to-end encrypted</span>
      </span>
      <span id="connection-status" className="titlebar-pill" role="status" data-connection-state={connection.state} data-error-code={connection.errorCode} aria-label={`Connection: ${label}`} title={label}>
        <span className="connection-dot" aria-hidden />
        <span className="titlebar-pill-label">{label}</span>
      </span>
    </>
  );
}

/* ------------------------------------------------------------------ app */

export function App() {
  const platform = detectPlatform();
  const [composerConversation] = useState(() =>
    typeof window === "undefined"
      ? null
      : readComposerConversation(window.location.search),
  );
  const [snapshot, setSnapshot] = useState<DesktopSnapshot | null>(null);
  const [selected, setSelected] = useState(composerConversation ?? "");
  const [attachmentViews, setAttachmentViews] = useState<
    Record<string, AttachmentView>
  >({});
  const [publicCopies, setPublicCopies] = useState<Record<string, PublicCopy>>(
    {},
  );
  const [theme, setTheme] = useState<Theme>("system");
  const [activeView, setActiveView] = useState<"conversations" | "notifications" | "settings">(
    "conversations",
  );
  const settingsOpen = activeView === "settings";
  const notificationsOpen = activeView === "notifications";
  const notificationSettingsRequested = useRef(false);
  const [notice, setNotice] = useState("");
  const [origin, setOrigin] = useState("");
  const [loading, setLoading] = useState(true);
  const [sending, setSending] = useState(false);
  const [, rerender] = useReducer((count: number) => count + 1, 0);
  const defaultListWidth = typeof window !== "undefined" && window.innerWidth < 900 ? 240 : 280;
  const [listWidth, setListWidth] = useState(defaultListWidth);
  const [listCollapsed, setListCollapsed] = useState(false);
  const [composerHeight, setComposerHeight] = useState<number | null>(null);
  const [viewportWidth, setViewportWidth] = useState(() =>
    typeof window === "undefined" ? 0 : window.innerWidth,
  );
  const [paneHeight, setPaneHeight] = useState(0);
  const [composerChrome, setComposerChrome] = useState(0);
  const mainRef = useRef<HTMLElement>(null);
  const listWidthLive = useRef(defaultListWidth);
  const listDragWidth = useRef(defaultListWidth);
  const previousListWidth = useRef(defaultListWidth);
  const composerHeightLive = useRef<number | null>(null);
  const persistedComposerHeight = useRef<number | null>(null);
  const paneRef = useRef<HTMLElement>(null);
  const desktopBodyRef = useRef<HTMLDivElement>(null);
  const messageListRef = useRef<HTMLDivElement>(null);
  const nearBottom = useRef(true);

  const selectedRef = useRef(selected);
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
  const sendingRef = useRef(false);
  const closingRef = useRef(false);
  const loadSequence = useRef(0);
  const messageRows = useRef(new Map<string, HTMLElement>());
  const handlers = useRef({
    rekeyed: (_from: string, _to: string) => {},
    close: () => {},
  });
  const storeRef = useRef<DraftStore | null>(null);
  if (!storeRef.current)
    storeRef.current = new DraftStore((input) => bridge.save_draft(input), {
      changed: () => rerender(),
      rekeyed: (from, to) => handlers.current.rekeyed(from, to),
    });
  const store = storeRef.current;

  const persistLayout = (
    next: {
      listWidth?: number;
      listCollapsed?: boolean;
      composerHeight?: number | null;
    } = {},
  ) => {
    try {
      localStorage.setItem(
        "openpush.layout.v1",
        JSON.stringify({
          listWidth: next.listWidth ?? listWidthLive.current,
          listCollapsed: next.listCollapsed ?? listCollapsed,
          composerHeight:
            "composerHeight" in next
              ? next.composerHeight
              : persistedComposerHeight.current,
        }),
      );
    } catch {
      /* Storage is optional in embedded previews. */
    }
  };
  const listMaxForWindow = Math.max(
    LIST_MIN_WIDTH,
    Math.min(
      LIST_MAX_WIDTH,
      viewportWidth - RAIL_WIDTH - 1 - PANE_MIN_WIDTH,
    ),
  );
  const renderedListWidth = Math.min(listWidth, listMaxForWindow);
  const setListSize = (value: number) => {
    if (value < LIST_COLLAPSE_THRESHOLD) {
      setListCollapsed(true);
      persistLayout({ listCollapsed: true });
      return;
    }
    const width = Math.max(LIST_MIN_WIDTH, Math.min(listMaxForWindow, value));
    listWidthLive.current = width;
    previousListWidth.current = width;
    setListWidth(width);
    setListCollapsed(false);
    persistLayout({ listWidth: width, listCollapsed: false });
  };
  const resizeList = (delta: number) => {
    listDragWidth.current += delta;
    setListSize(listDragWidth.current);
  };
  const resizeListTo = (value: number) => {
    listDragWidth.current = value;
    setListSize(value);
  };
  const commitList = () =>
    persistLayout({
      listCollapsed: listDragWidth.current < LIST_COLLAPSE_THRESHOLD,
      listWidth: listWidthLive.current,
    });
  const composerMinimum = () =>
    document.getElementById("attachment-tray")
      ? COMPOSER_ATTACHMENT_MIN_HEIGHT
      : COMPOSER_MIN_HEIGHT;
  const composerMax = () =>
    Math.max(
      composerMinimum(),
      (paneRef.current?.offsetHeight ?? window.innerHeight) * 0.5,
    );
  const resizeComposer = (delta: number) => {
    const current = composerHeightLive.current ?? composerMinimum();
    const height = Math.max(
      composerMinimum(),
      Math.min(composerMax(), current - delta),
    );
    composerHeightLive.current = height;
    setComposerHeight(height);
  };
  const resizeComposerTo = (value: number) => {
    const height = Math.max(composerMinimum(), Math.min(composerMax(), value));
    composerHeightLive.current = height;
    setComposerHeight(height);
  };
  const commitComposer = () => {
    persistedComposerHeight.current = composerHeightLive.current;
    persistLayout({ composerHeight: composerHeightLive.current });
  };

  useEffect(() => {
    try {
      const saved = JSON.parse(
        localStorage.getItem("openpush.layout.v1") ?? "{}",
      ) as {
        listWidth?: number;
        listCollapsed?: boolean;
        composerHeight?: number | null;
      };
      if (typeof saved.listWidth === "number") {
        const width = Math.max(
          LIST_MIN_WIDTH,
          Math.min(LIST_MAX_WIDTH, saved.listWidth),
        );
        listWidthLive.current = width;
        listDragWidth.current = width;
        previousListWidth.current = width;
        setListWidth(width);
      }
      if (typeof saved.listCollapsed === "boolean") setListCollapsed(saved.listCollapsed);
      if (typeof saved.composerHeight === "number") {
        persistedComposerHeight.current = saved.composerHeight;
        resizeComposerTo(saved.composerHeight);
      }
    } catch { /* Corrupt persisted layout falls back to defaults. */ }
    const updateWindowMeasurements = () => {
      setViewportWidth(window.innerWidth);
      const height = paneRef.current?.offsetHeight ?? 0;
      setPaneHeight((current) => (current === height ? current : height));
      if (composerHeightLive.current !== null) resizeComposerTo(composerHeightLive.current);
    };
    updateWindowMeasurements();
    window.addEventListener("resize", updateWindowMeasurements);
    return () => window.removeEventListener("resize", updateWindowMeasurements);
  }, []);
  useEffect(() => {
    if (composerConversation !== null) return;
    void bridge.set_notification_context(activeView, activeView === "conversations" ? selectedRef.current : undefined).catch(() => {});
  }, [activeView, selected, composerConversation]);
  useEffect(() => {
    if (activeView === "settings" && notificationSettingsRequested.current) {
      notificationSettingsRequested.current = false;
      document.getElementById("notification-settings")?.scrollIntoView?.({ block: "start" });
    }
  }, [activeView]);
  useEffect(() => installOverlayScrollbars(document), []);
  useLayoutEffect(() => {
    const measure = () => {
      const height = paneRef.current?.offsetHeight ?? 0;
      setPaneHeight((current) => (current === height ? current : height));
      const composerArea = document.getElementById("composer-area");
      const textarea = document.getElementById("composer-textarea");
      if (!(composerArea instanceof HTMLElement) || !(textarea instanceof HTMLElement)) return;
      const chrome = composerArea.offsetHeight - textarea.offsetHeight;
      setComposerChrome((current) => (current === chrome ? current : chrome));
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    if (paneRef.current) observer.observe(paneRef.current);
    const composerArea = document.getElementById("composer-area");
    if (composerArea) observer.observe(composerArea);
    return () => observer.disconnect();
  }, [activeView, loading, Boolean(snapshot), snapshot?.connection.state, Boolean(selected)]);
  useEffect(() => {
    const root = mainRef.current;
    if (!root) return;
    const focus = () => { root.dataset.windowFocused = "true"; };
    const blur = () => { root.dataset.windowFocused = "false"; };
    root.dataset.windowFocused = document.hasFocus() ? "true" : "false";
    window.addEventListener("focus", focus);
    window.addEventListener("blur", blur);
    return () => {
      window.removeEventListener("focus", focus);
      window.removeEventListener("blur", blur);
    };
  }, []);

  const choose = (key: string) => {
    selectedRef.current = key;
    setSelected(key);
  };
  const report = (prefix: string) => (error: unknown) =>
    setNotice(`${prefix}${errorText(error)}`);

  /** Loads host state for one conversation; responses superseded by a newer request are dropped. */
  const refresh = async (conversationId = selectedRef.current) => {
    const sequence = ++loadSequence.current;
    const state = await bridge.load_state(
      conversationId && !isLocalDraftKey(conversationId)
        ? conversationId
        : undefined,
    );
    if (sequence !== loadSequence.current) return;
    setSnapshot(state);
    if (state.activeConversationId)
      store.adopt(state.activeConversationId, state.draft);
    if (
      !composerConversation &&
      !selectedRef.current &&
      state.activeConversationId
    )
      choose(state.activeConversationId);
  };

  const select = (key: string) => {
    const previous = selectedRef.current;
    if (key === previous) return;
    if (previous) void store.flush(previous);
    choose(key);
    setNotice("");
    if (!isLocalDraftKey(key)) void refresh(key).catch(report(""));
  };

  handlers.current.rekeyed = (from, to) => {
    if (selectedRef.current !== from) return;
    choose(to);
    void refresh(to).catch(report(""));
  };

  useEffect(() => {
    void refresh(composerConversation ?? undefined)
      .catch(report(""))
      .finally(() => setLoading(false));
    return bridge.subscribe(() => void refresh().catch(report("")));
  }, []);

  /* Only rows that are actually visible while the window is focused are marked seen. */
  const slotKey = store.resolve(selected);
  const active = snapshot?.conversations.find(
    (conversation) => conversation.id === slotKey,
  );
  const conversationLoaded =
    Boolean(active) && snapshot?.activeConversationId === slotKey;
  const messageIds = conversationLoaded
    ? active!.messages.map((message) => message.id).join(",")
    : "";
  const stickToBottom = (element = messageListRef.current) => {
    if (!element) return;
    element.setAttribute("data-scroll-programmatic", "");
    element.scrollTop = element.scrollHeight;
    if (typeof requestAnimationFrame === "undefined") {
      element.removeAttribute("data-scroll-programmatic");
      return;
    }
    requestAnimationFrame(() =>
      element.removeAttribute("data-scroll-programmatic"),
    );
  };
  const onMessageScroll = () => {
    const element = messageListRef.current;
    if (!element) return;
    nearBottom.current =
      element.scrollHeight - element.scrollTop - element.clientHeight <= 48;
  };
  useLayoutEffect(() => {
    if (!conversationLoaded) return;
    stickToBottom();
    nearBottom.current = true;
  }, [slotKey, conversationLoaded]);
  useLayoutEffect(() => {
    if (nearBottom.current) stickToBottom();
  }, [messageIds]);
  useLayoutEffect(() => {
    const list = messageListRef.current;
    const content = document.getElementById("message-list-content");
    if (!list || !content || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (nearBottom.current) stickToBottom(list);
    });
    observer.observe(list);
    observer.observe(content);
    return () => observer.disconnect();
  }, [slotKey, conversationLoaded]);
  useEffect(() => {
    const seen = new Set<string>();
    const markVisible = () => {
      if (!document.hasFocus() || document.visibilityState !== "visible")
        return;
      const ids = [...messageRows.current]
        .filter(([id, row]) => row.dataset.visible === "true" && !seen.has(id))
        .map(([id]) => id);
      if (!ids.length) return;
      ids.forEach((id) => seen.add(id));
      void bridge.mark_seen(ids).catch(report(""));
    };
    const observer =
      typeof IntersectionObserver === "undefined"
        ? undefined
        : new IntersectionObserver((entries) => {
            entries.forEach((entry) =>
              entry.target.setAttribute(
                "data-visible",
                String(entry.isIntersecting),
              ),
            );
            markVisible();
          });
    messageRows.current.forEach((row) => observer?.observe(row));
    window.addEventListener("focus", markVisible);
    document.addEventListener("visibilitychange", markVisible);
    markVisible();
    return () => {
      observer?.disconnect();
      window.removeEventListener("focus", markVisible);
      document.removeEventListener("visibilitychange", markVisible);
    };
  }, [slotKey, messageIds]);
  const registerRow: RowRegistry = (id, row) => {
    if (row) messageRows.current.set(id, row);
    else messageRows.current.delete(id);
  };

  const slot = store.get(slotKey);
  const content = slot?.content ?? blankContent();
  const gateways = snapshot?.gateways ?? [];
  const route = resolveRoute(content, gateways);
  const mmsRequired = requiresMms(content, active);
  const routeRecipients = recipientCount(content, active);
  const estimateBytes = draftEstimateBytes(content, attachmentViews);
  const problem = routeProblem(
    route,
    gateways,
    mmsRequired,
    routeRecipients,
    estimateBytes,
  );
  const isNewConversation =
    Boolean(slotKey) &&
    (isLocalDraftKey(slotKey) ||
      (conversationLoaded && active!.messages.length === 0));

  const edit = (patch: Partial<DraftContent>) => {
    if (slotKey) void store.edit(slotKey, patch);
  };

  const startNewMessage = (recipient: string) => {
    const previous = selectedRef.current;
    if (previous) void store.flush(previous);
    const { key } = store.createNew(recipient);
    choose(store.resolve(key));
    setNotice("");
  };

  const retrySave = async () => {
    const current = store.get(slotKey);
    if (!current) return;
    if (current.error?.code === "stale-draft" && current.conversationId) {
      try {
        const state = await bridge.load_state(current.conversationId);
        store.rebase(
          slotKey,
          state.activeConversationId === current.conversationId
            ? state.draft
            : undefined,
        );
      } catch (error) {
        return setNotice(errorText(error));
      }
    }
    await store.flush(slotKey);
  };

  const addAttachment = async () => {
    if (!slotKey) return;
    try {
      const picked = await bridge.pick_attachments();
      if (!picked.length) return;
      setAttachmentViews((current) => ({
        ...current,
        ...Object.fromEntries(picked.map((file) => [file.id, file])),
      }));
      const existing = store.get(slotKey)?.content.attachmentIds ?? [];
      void store.edit(slotKey, {
        attachmentIds: [...existing, ...picked.map((file) => file.id)],
      });
    } catch (error) {
      setNotice(`Attachment picker: ${errorText(error)}`);
    }
  };

  const send = async () => {
    if (sendingRef.current || !slotKey) return;
    sendingRef.current = true;
    setSending(true);
    setNotice("");
    try {
      if (active?.replyBlockedReason)
        return setNotice(`Not sent: ${active.replyBlockedReason}.`);
      const flushed = await store.flush(slotKey);
      if (!flushed.ok)
        return setNotice(
          `Not sent: the draft could not be saved (${flushed.error}).`,
        );
      const current = store.get(slotKey);
      if (!current?.id)
        return setNotice("Not sent: the draft has not been stored yet.");
      const latestSnapshot = snapshotRef.current;
      const latestConversation = latestSnapshot?.conversations.find(
        (conversation) => conversation.id === current.conversationId,
      );
      if (latestConversation?.replyBlockedReason)
        return setNotice(`Not sent: ${latestConversation.replyBlockedReason}.`);
      const latestGateways = latestSnapshot?.gateways ?? [];
      const chosen = resolveRoute(current.content, latestGateways);
      const blocked = routeProblem(
        chosen,
        latestGateways,
        requiresMms(current.content, latestConversation),
        recipientCount(current.content, latestConversation),
        draftEstimateBytes(current.content, attachmentViews),
      );
      if (blocked || chosen.kind !== "route")
        return setNotice(`Not sent: ${blocked ?? "choose a gateway and SIM"}.`);
      const sentGeneration = current.generation;
      store.hold(current.key);
      try {
        const result = await bridge.send_draft({
          id: current.id,
          conversationId: current.conversationId,
          ...current.content,
          expectedRevision: current.revision,
          gatewayId: chosen.gateway.id,
          simId: chosen.gateway.simId,
        });
        if (!result.accepted) {
          store.release(current.key);
          return setNotice(
            `${STATUS_LABEL[result.status]}: ${result.reason ?? "The draft was preserved."}`,
          );
        }
        store.markSent(current.key, sentGeneration);
      } catch (error) {
        store.release(current.key);
        return setNotice(`Send failed: ${errorText(error)}`);
      }
      await refresh(current.conversationId).catch(report(""));
    } finally {
      sendingRef.current = false;
      setSending(false);
    }
  };

  const publish = async (attachment: AttachmentView) => {
    try {
      const copy = await bridge.publish_attachment(attachment.id);
      if (!copy)
        return setNotice("Public link not created; nothing was shared.");
      setPublicCopies((current) => ({ ...current, [attachment.id]: copy }));
    } catch (error) {
      setNotice(`Public link: ${errorText(error)}`);
    }
  };
  const retryAttachment = async (attachment: AttachmentView) => {
    try {
      await bridge.retry_attachment(attachment.id);
      setNotice(`Retrying ${attachment.transfer ?? "attachment"} transfer.`);
    } catch (error) {
      setNotice(`Attachment retry: ${errorText(error)}`);
    }
  };
  const saveAttachment = async (attachment: AttachmentView) => {
    try {
      if (!await bridge.save_attachment(attachment.id))
        setNotice("Attachment save cancelled.");
    } catch (error) {
      setNotice(`Attachment save: ${errorText(error)}`);
    }
  };

  /** Closing never discards an unsaved draft: a failed save keeps the window open. */
  const closeAfterSave = async (close: () => Promise<void>) => {
    if (closingRef.current) return;
    closingRef.current = true;
    try {
      const outcome = selectedRef.current
        ? await store.flush(selectedRef.current)
        : { ok: true as const };
      if (!outcome.ok)
        return setNotice(
          `The window stayed open because the draft was not saved (${outcome.error}).`,
        );
      await close();
    } catch (error) {
      setNotice(errorText(error));
    } finally {
      closingRef.current = false;
    }
  };
  handlers.current.close = () =>
    void closeAfterSave(() => bridge.close_composer());

  const openComposerWindow = async (conversationId?: string) => {
    if (conversationId) {
      const outcome = await store.flush(conversationId);
      if (!outcome.ok)
        return setNotice(
          `Not opened: the draft could not be saved (${outcome.error}).`,
        );
    }
    try {
      await bridge.open_composer(conversationId);
    } catch (error) {
      setNotice(`Composer window: ${errorText(error)}`);
    }
  };

  const setup = async (action: "origin" | "credentials" | "unlock") => {
    try {
      if (action === "origin") await bridge.configure_server(origin);
      else if (action === "credentials") await bridge.import_credentials();
      else await bridge.unlock_sync();
      await refresh();
    } catch (error) {
      setNotice(errorText(error));
    }
  };

  useEffect(() => {
    if (!composerConversation) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.isComposing)
        handlers.current.close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const conversations: Conversation[] = [
    ...store.localDrafts().map((local) => ({
      id: local.key,
      name: local.content.recipientIds.join(", ") || "New message",
      preview: "Not saved yet",
      unread: 0,
    })),
    ...(snapshot?.conversations ?? []).map(({ id, name, preview, unread }) => ({
      id,
      name,
      preview,
      unread,
    })),
  ];

  if (loading)
    return (
      <main
        ref={mainRef}
        id="desktop-shell"
        aria-busy="true"
        data-platform={platform}
      >
        Loading messaging state…
      </main>
    );

  const setupCode = snapshot?.connection.errorCode;
  const canUnlock =
    snapshot?.mode === "native" &&
    snapshot.encryption.state !== "unlocked" &&
    !["server-required", "credentials-required", "revoked"].includes(
      setupCode ?? "",
    );
  const attachments: Attachment[] = content.attachmentIds.map(
    (id) =>
      attachmentViews[id] ?? { id, name: "Attached file", state: "pending" },
  );
  const title =
    active?.name ??
    (isLocalDraftKey(slotKey)
      ? content.recipientIds.join(", ") || "New message"
      : undefined);
  const maxAutoGrowHeight = paneHeight
    ? Math.max(
        TEXTAREA_LINE_HEIGHT,
        Math.min(
          TEXTAREA_MAX_HEIGHT,
          Math.floor(paneHeight * 0.5) - composerChrome,
        ),
      )
    : undefined;
  const composer = (
    <section
      id="composer-area"
      aria-label="Composer"
      aria-busy={sending}
      data-user-sized={composerHeight !== null || undefined}
      style={composerHeight === null ? undefined : { height: composerHeight }}
    >
      {isNewConversation && (
        <RecipientField
          key={slotKey}
          recipients={content.recipientIds}
          onCommit={(recipientIds) => edit({ recipientIds })}
        />
      )}
      {active?.replyBlockedReason && (
        <p id="reply-blocked-reason" data-reply-blocked role="status">Replies unavailable: {active.replyBlockedReason}</p>
      )}
      {slot?.error && (
        <DraftRecovery
          error={slot.error.message}
          onRetry={() => void retrySave()}
        />
      )}
      <Composer
        draft={content.text}
        attachments={attachments}
        sendSupported={Boolean(slotKey) && !problem && !active?.replyBlockedReason && !sending}
        unavailableReason={sending ? "sending…" : active?.replyBlockedReason ?? problem}
        onDraftChange={(text) => edit({ text })}
        onSend={() => void send()}
        onAddAttachment={slotKey ? () => void addAttachment() : undefined}
        onRemoveAttachment={slotKey ? (id) => edit({ attachmentIds: content.attachmentIds.filter((attachmentId) => attachmentId !== id) }) : undefined}
        status={notice}
        composerName={title}
        composerUserSized={composerHeight !== null}
        maxAutoGrowHeight={maxAutoGrowHeight}
        platform={platform}
        gatewaySlot={snapshot ? (
          <GatewaySelector
            gateways={gateways}
            content={content}
            problem={active?.replyBlockedReason ?? problem}
            mmsRequired={mmsRequired}
            estimatedBytes={estimateBytes}
            onSelect={(gateway) =>
              edit({ gatewayId: gateway.id, simId: gateway.simId })
            }
          />
        ) : null}
      />
    </section>
  );
  const messages = (
    <MessageList
      conversation={active}
      loading={Boolean(active) && !conversationLoaded}
      registerRow={registerRow}
      publicCopies={publicCopies}
      onPublish={(file) => void publish(file)}
      onRetryAttachment={(file) => void retryAttachment(file)}
      onSaveAttachment={(file) => void saveAttachment(file)}
      listRef={messageListRef}
      onScroll={onMessageScroll}
    />
  );
  if (composerConversation) {
    return (
      <main
        ref={mainRef}
        id="composer-shell"
        className={`theme-${theme}`}
        data-bridge-mode={snapshot?.mode ?? "unavailable"}
        data-platform={platform}
      >
        <section
          id="conversation-pane"
          className="conversation-pane"
          aria-label="Conversation"
          ref={paneRef}
        >
          <AppTitlebar
            isComposer
            platform={platform}
            title={title}
            status={snapshot && <TitlebarStatus connection={snapshot.connection} encryption={snapshot.encryption.state} />}
            onMinimize={() => {}}
            onMaximize={() => {}}
            onClose={handlers.current.close}
          />
          {messages}
          <ResizeHandle
            id="handle-h2"
            direction="vertical"
            ariaLabel="Resize composer"
            value={composerHeight ?? composerMinimum()}
            min={composerMinimum()}
            max={composerMax()}
            valueUnit="pixels"
            onResize={resizeComposer}
            onResizeTo={resizeComposerTo}
            onResizeEnd={commitComposer}
            onDoubleClick={() => {
              composerHeightLive.current = null;
              persistedComposerHeight.current = null;
              setComposerHeight(null);
              persistLayout({ composerHeight: null });
            }}
          />
          {composer}
        </section>
      </main>
    );
  }

  return (
    <main
      ref={mainRef}
      id="desktop-shell"
      className={`theme-${theme}`}
      data-bridge-mode={snapshot?.mode ?? "unavailable"}
      data-platform={platform}
    >
      <AppTitlebar
        onMinimize={() => void bridge.window("minimize")}
        onMaximize={() => void bridge.window("maximize")}
        onClose={() => void closeAfterSave(() => bridge.window("close"))}
        platform={platform}
        status={snapshot && <TitlebarStatus connection={snapshot.connection} encryption={snapshot.encryption.state} />}
      />
      <div ref={desktopBodyRef} id="desktop-body" className="desktop-layout">
        <Panel
          activeView={activeView}
          onView={setActiveView}
          onToggleList={() =>
            resizeListTo(listCollapsed ? previousListWidth.current : 0)
          }
          listCollapsed={listCollapsed}
          threadListId="thread-list"
          connectionLabel={
            snapshot ? connectionText(snapshot.connection) : "Loading"
          }
          connectionState={snapshot?.connection.state ?? "offline"}
          notificationUnread={snapshot?.notifications.filter(notification => !notification.seen && !notification.dismissalPending && !snapshot.appFilters.some(filter => filter.muted && filter.sourceDeviceId === notification.target.sourceDeviceId && filter.packageName === notification.packageName)).length ?? 0}
        />
        <aside
          id="thread-list"
          aria-label="Thread list"
          data-collapsed={listCollapsed || undefined}
          hidden={settingsOpen || notificationsOpen}
          style={
            listCollapsed || settingsOpen || notificationsOpen
              ? { display: "none" }
              : { width: renderedListWidth, flexBasis: renderedListWidth }
          }
        >
          <div
            id="thread-list-toolbar"
            role="toolbar"
            aria-label="Thread list toolbar"
          >
            <RecipientPicker
              recipients={conversations.filter(
                (item) => !isLocalDraftKey(item.id),
              )}
              onChange={(ids) => ids[0] && select(ids[0])}
              onNewRecipient={startNewMessage}
            />
            <button
              aria-label="New message"
              title="New message"
              data-action="new-message"
              onClick={() =>
                document.getElementById("recipient-search")?.focus()
              }
            >
              <MessageSquarePlus size={18} aria-hidden />
            </button>
          </div>
          <ConversationList
            conversations={conversations}
            selectedId={slotKey}
            onSelect={select}
          />
        </aside>
        {!settingsOpen && !notificationsOpen && <ResizeHandle
          id="handle-h1"
          direction="horizontal"
          ariaLabel="Resize thread list"
          value={listCollapsed ? 0 : renderedListWidth}
          min={LIST_MIN_WIDTH}
          max={listMaxForWindow}
          valueUnit="pixels"
          onResizeStart={(position) => {
            const bodyLeft =
              desktopBodyRef.current?.getBoundingClientRect().left ?? 0;
            listDragWidth.current = listCollapsed
              ? position - (bodyLeft + RAIL_WIDTH)
              : renderedListWidth;
          }}
          onResize={resizeList}
          onResizeTo={resizeListTo}
          onResizeEnd={commitList}
          collapsed={listCollapsed ? "before" : undefined}
          collapsible={{ side: "before", restoreValue: previousListWidth.current }}
        />}
        <section
          id="conversation-pane"
          className="conversation-pane"
          aria-label={settingsOpen ? "Settings pane" : notificationsOpen ? "Notifications pane" : "Conversation"}
          data-view={activeView}
          ref={paneRef}
        >
          <header id="thread-pane-header">
            <div className="header-copy">
              {settingsOpen ? (
                <h1 data-header-title>Settings</h1>
              ) : notificationsOpen ? (
                <h1 data-header-title>Notifications</h1>
              ) : (
                <div className="header-copy-details">
                  <b data-header-title>{title ?? "Set up OpenPush"}</b>
                  {active?.participants && <small data-conversation-participants>{active.participants.join(", ")}</small>}
                </div>
              )}
            </div>
            <div className="header-actions" hidden={settingsOpen || notificationsOpen}>
              <button
                id="new-composer-window"
                aria-label="New message window"
                title="New message window"
                onClick={() => void openComposerWindow()}
              >
                <SquarePen size={16} aria-hidden />
              </button>
              {conversationLoaded && (
                <button
                  aria-label="Open in composer window"
                  title="Open in composer window"
                  onClick={() => void openComposerWindow(slotKey)}
                >
                  <ExternalLink size={16} aria-hidden />
                </button>
              )}
            </div>
          </header>
          {settingsOpen ? (
            <SettingsView
              origin={origin}
              onOrigin={setOrigin}
              onAction={(action) => void setup(action)}
              encryption={snapshot?.encryption.state ?? "locked"}
              canUnlock={canUnlock}
              theme={theme}
              onTheme={setTheme}
              headStatus={snapshot?.head.note ?? "Normal main-window fallback"}
              notifications={snapshot?.notifications ?? []}
              filters={snapshot?.appFilters ?? []}
              sources={snapshot?.gateways ?? []}
              preferences={snapshot?.notificationPreferences ?? { messageBanners: true, mirroredBanners: true, preview: "full" }}
              onPreferences={preferences => bridge.set_notification_preferences(preferences).then(() => refresh()).catch(report("Could not save notification preferences. "))}
              onMute={filter => bridge.set_app_muted(filter.sourceDeviceId, filter.packageName, filter.appName, filter.muted).then(() => refresh()).catch(report("Could not save app filter. "))}
              onPermission={() => bridge.request_notification_permission().then(result => setNotice(result === "unknown" ? "Check OpenPush in your operating system notification settings; permission cannot be read here." : `Desktop notification permission: ${result}.`)).catch(report("Could not check desktop notifications. "))}
            />
          ) : notificationsOpen ? (
            <NotificationsView
              notifications={snapshot?.notifications ?? []}
              filters={snapshot?.appFilters ?? []}
              sources={snapshot?.gateways ?? []}
              locked={snapshot?.encryption.state !== "unlocked"}
              onDismiss={target => void bridge.dismiss_notification(target).then(() => refresh()).catch(report("Could not dismiss notification. "))}
              onDismissAll={() => void bridge.dismiss_all_notifications().then(() => refresh()).catch(report("Could not dismiss notifications. "))}
              onMute={filter => void bridge.set_app_muted(filter.sourceDeviceId, filter.packageName, filter.appName, true).then(() => refresh()).catch(report("Could not mute app. "))}
              onSeen={targets => bridge.mark_notifications_seen(targets).then(() => refresh()).catch(error => { report("")(error); throw error; })}
              onSettings={() => { notificationSettingsRequested.current = true; setActiveView("settings"); }}
            />
          ) : (
            <>
              {snapshot &&
              snapshot.connection.state !== "connected" &&
              !selected ? (
                <OnboardingView
                  connected={false}
                  canUnlock={canUnlock}
                  origin={origin}
                  onOrigin={setOrigin}
                  onAction={(action) => void setup(action)}
                  encryption={snapshot.encryption.state}
                />
              ) : (
                <>
                  {messages}
                  <ResizeHandle
                    id="handle-h2"
                    direction="vertical"
                    ariaLabel="Resize composer"
                    value={composerHeight ?? composerMinimum()}
                    min={composerMinimum()}
                    max={composerMax()}
                    valueUnit="pixels"
                    onResize={resizeComposer}
                    onResizeTo={resizeComposerTo}
                    onResizeEnd={commitComposer}
                    onDoubleClick={() => {
                      composerHeightLive.current = null;
                      persistedComposerHeight.current = null;
                      setComposerHeight(null);
                      persistLayout({ composerHeight: null });
                    }}
                  />
                  {composer}
                </>
              )}
            </>
          )}
        </section>
      </div>
    </main>
  );
}
