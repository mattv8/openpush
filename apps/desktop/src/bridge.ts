/**
 * UI-only native boundary. These DTOs intentionally contain no credentials,
 * cryptographic material, device tokens, or raw filesystem paths.
 */
export type ConnectionState = "connected" | "offline" | "missing-native-host" | "error";
export type MessageStatus = "queued-local" | "server-accepted" | "gateway-persisted" | "preparing" | "submitted" | "sent" | "delivery-confirmed" | "failed-before-submit" | "failed-confirmed" | "unknown";
export type AttachmentState = "pending" | "uploading" | "failed" | "ready";
export type AttachmentView = { id: string; name: string; mediaType: string; byteSize: number; state: AttachmentState; error?: string; previewUrl?: string; transfer?: "upload" | "download"; retryable?: boolean };
export type MessageView = { id: string; revision: string; sender: "self" | "other"; body: string; timestamp: string; status?: MessageStatus; attachments: AttachmentView[]; transport?: "sms" | "mms"; subject?: string; participants?: string[] };
/** `participants` are native-provided reply addressees and exclude a confirmed self address. */
export type ConversationView = { id: string; name: string; preview: string; unread: number; messages: MessageView[]; participants?: string[]; replyBlockedReason?: string };
export type GatewayView = { id: string; name: string; simId: string; online: boolean; simulated: boolean; supportsSms: boolean; supportsMms: boolean; capabilityNote?: string; mmsContentVersion?: number; mmsMaxBytes?: number; mmsLimitSource?: "carrier" | "fallback"; mmsMaxRecipients?: number };
export type Draft = { id: string; conversationId: string; text: string; recipientIds: string[]; attachmentIds: string[]; gatewayId?: string; simId?: string; revision: string };
export type DesktopSnapshot = { version: "1"; mode: "fixture" | "native"; connection: { state: ConnectionState; origin?: string; errorCode?: string }; encryption: { state: "locked" | "unlocked" | "preview" | "mismatch"; profileFingerprint?: string }; gateways: GatewayView[]; conversations: ConversationView[]; activeConversationId?: string; draft?: Draft; head: { enabled: boolean; capability: "supported" | "unsupported" | "unconfirmed"; note?: string }; pendingCount: number; quarantineCount: number; notifications: MirroredNotification[]; appFilters: AppFilter[]; notificationPreferences: NotificationPreferences };
/** gatewayId/simId are optional on saves (omitted = keep the stored route) and required on sends. */
export type DraftInput = Pick<Draft, "id" | "conversationId" | "text" | "recipientIds" | "attachmentIds"> & { expectedRevision: string; gatewayId?: string; simId?: string };
export type SendDraftInput = DraftInput & { gatewayId: string; simId: string };
/** A separate, server-readable re-encoded copy created only after native confirmation. */
export type PublicCopy = { url: string; expiresInSeconds: number };
export type NotificationTarget = { sourceDeviceId: string; notificationKey: string; lifetime: string };
export type MirroredNotification = { target: NotificationTarget; packageName: string; appName: string; title: string; text: string; category?: string; postedAt: number; dismissible: boolean; seen: boolean; dismissalPending: boolean };
export type AppFilter = { sourceDeviceId: string; packageName: string; appName: string; muted: boolean };
export type NotificationPreferences = { messageBanners: boolean; mirroredBanners: boolean; preview: "full" | "hidden" };
/** `currentRevision` is set when the stored draft revision is known (stale saves/sends, refused sends). */
export type BridgeError = { code: string; message: string; currentRevision?: string };

export interface DesktopBridge {
  load_state(conversationId?: string): Promise<DesktopSnapshot>;
  configure_server(origin: string): Promise<void>;
  import_credentials(): Promise<void>;
  unlock_sync(): Promise<void>;
  save_draft(input: DraftInput): Promise<Draft>;
  /** `accepted` means durably queued in the local encrypted outbox; `revision` is the stored draft revision afterwards. */
  send_draft(input: SendDraftInput): Promise<{ accepted: boolean; status: MessageStatus; reason?: string; revision?: string }>;
  mark_seen(visibleMessageIds: string[]): Promise<void>;
  dismiss_notification(target: NotificationTarget): Promise<void>;
  dismiss_all_notifications(): Promise<void>;
  set_app_muted(sourceDeviceId: string, packageName: string, appName: string, muted: boolean): Promise<void>;
  mark_notifications_seen(targets: NotificationTarget[]): Promise<void>;
  set_notification_preferences(preferences: NotificationPreferences): Promise<void>;
  set_notification_context(view: "conversations" | "notifications" | "settings", conversationId?: string): Promise<void>;
  request_notification_permission(): Promise<"granted" | "denied" | "unknown">;
  pick_attachments(): Promise<AttachmentView[]>;
  retry_attachment(id: string): Promise<void>;
  save_attachment(id: string): Promise<boolean>;
  /** Resolves null when the person cancels the native confirmation. */
  publish_attachment(id: string): Promise<PublicCopy | null>;
  /** Explicit user action only; omitting the ID starts a new-conversation draft. */
  open_composer(conversationId?: string): Promise<void>;
  show_head(conversationId: string): Promise<void>;
  update_head(conversationId: string): Promise<void>;
  hide_head(conversationId: string): Promise<void>;
  close_composer(): Promise<void>;
  subscribe(listener: () => void): () => void;
  window(action: "minimize" | "maximize" | "close"): Promise<void>;
}

const fixtureMessages: MessageView[] = [
  { id: "message-aurora-1", revision: "1", sender: "other", body: "Can you send over the estimate?", timestamp: "09:41", attachments: [] },
  { id: "message-aurora-2", revision: "2", sender: "self", body: "I'll have it to you shortly.", timestamp: "09:43", status: "sent", attachments: [] },
];
const fixtureSnapshot: DesktopSnapshot = { version: "1", mode: "fixture", connection: { state: "connected", origin: "https://push.example.com" }, encryption: { state: "unlocked" }, gateways: [{ id: "gateway-pixel8", name: "Pixel 8", simId: "sim-1", online: true, simulated: true, supportsSms: true, supportsMms: true, mmsContentVersion: 2, mmsMaxBytes: 300 * 1024, mmsLimitSource: "fallback", mmsMaxRecipients: 20 }], conversations: [{ id: "conv-aurora", name: "Aurora Chen", preview: "Can you send over the estimate?", unread: 2, messages: fixtureMessages }, { id: "conv-river", name: "River Park", preview: "Attachment received", unread: 0, participants: ["River Park", "Mina Torres"], messages: [{ id: "message-river-1", revision: "1", sender: "other", body: "Attachment received", timestamp: "Yesterday", transport: "mms", subject: "Estimate", attachments: [{ id: "attachment-river-1", name: "estimate.pdf", mediaType: "application/pdf", byteSize: 182000, state: "ready", transfer: "download", retryable: true }] }] }], activeConversationId: "conv-aurora", draft: undefined, head: { enabled: false, capability: "unsupported" }, pendingCount: 1, quarantineCount: 0, notifications: [{ target: { sourceDeviceId: "gateway-pixel8", notificationKey: "chat-42", lifetime: "1" }, packageName: "com.example.chat", appName: "Chat", title: "Morgan", text: "Are we still meeting after lunch?", postedAt: Date.now() - 120000, dismissible: true, seen: false, dismissalPending: false }, { target: { sourceDeviceId: "gateway-pixel8", notificationKey: "mail-7", lifetime: "1" }, packageName: "com.example.mail", appName: "Mail", title: "Project update", text: "A detailed update is ready for your review.", postedAt: Date.now() - 3600000, dismissible: true, seen: false, dismissalPending: false }], appFilters: [], notificationPreferences: { messageBanners: true, mirroredBanners: true, preview: "full" } };
/** Fixture drafts keyed by conversation ID; mirrors the native compose-draft save contract (session.rs). */
const fixtureDrafts = new Map<string, Draft>();
let fixtureCreated = 0;
const fixtureError = (code: string, message: string): BridgeError => ({ code, message });
const fixtureConversationIds = () => [...fixtureSnapshot.conversations.map(c => c.id), ...fixtureDrafts.keys()];
/** Draft-only conversations are listed like the host lists them. */
const fixtureDraftConversations = (): ConversationView[] => [...fixtureDrafts.values()]
  .filter(draft => !fixtureSnapshot.conversations.some(c => c.id === draft.conversationId))
  .map(draft => ({ id: draft.conversationId, name: draft.recipientIds.join(", ") || "New message", preview: draft.text ? `Draft: ${draft.text.slice(0, 80)}` : "Draft", unread: 0, messages: [] }));
/**
 * Empty draft ID: create the draft (or attach to the conversation's existing one); an empty
 * conversation ID also creates a new conversation. Saves are CAS on a numeric revision.
 */
const fixtureSave = (input: DraftInput): Draft => {
  const expected = Number(input.expectedRevision);
  if (!/^\d+$/.test(input.expectedRevision)) throw fixtureError("invalid-draft", "The draft revision is invalid.");
  let current: Draft | undefined;
  if (input.id === "") {
    const conversationId = input.conversationId || `conv-fixture-new-${++fixtureCreated}`;
    current = fixtureDrafts.get(conversationId) ?? { id: `draft-fixture-${conversationId}`, conversationId, text: "", recipientIds: [], attachmentIds: [], revision: "0" };
  } else {
    current = [...fixtureDrafts.values()].find(draft => draft.id === input.id);
    if (!current) throw fixtureError("not-found", "The requested stored item was not found.");
  }
  if (input.conversationId && input.conversationId !== current.conversationId) throw fixtureError("invalid-draft", "The draft does not belong to this conversation.");
  if (Number(current.revision) !== expected) throw fixtureError("stale-draft", `The draft changed elsewhere (current revision ${current.revision}); both versions were kept.`);
  if (Boolean(input.gatewayId) !== Boolean(input.simId)) throw fixtureError("invalid-route", "Select a gateway and SIM together.");
  const saved: Draft = {
    id: current.id, conversationId: current.conversationId, text: input.text,
    recipientIds: input.recipientIds.map(r => r.trim()),
    attachmentIds: input.attachmentIds,
    gatewayId: input.gatewayId || current.gatewayId, simId: input.simId || current.simId,
    revision: String(expected + 1),
  };
  fixtureDrafts.set(saved.conversationId, saved);
  return saved;
};
export const fixtureBridge: DesktopBridge = {
  load_state: async conversationId => {
    const active = conversationId && fixtureConversationIds().includes(conversationId) ? conversationId : fixtureSnapshot.activeConversationId!;
    return { ...fixtureSnapshot, conversations: [...fixtureSnapshot.conversations, ...fixtureDraftConversations()], activeConversationId: active, draft: fixtureDrafts.get(active) };
  },
  configure_server: async () => undefined,
  import_credentials: async () => undefined,
  unlock_sync: async () => undefined,
  save_draft: async input => fixtureSave(input),
  send_draft: async () => ({ accepted: false, status: "failed-before-submit", reason: "Fixture gateway is offline; the draft was preserved." }),
  mark_seen: async () => undefined,
  dismiss_notification: async target => { const item = fixtureSnapshot.notifications.find(notification => notification.target.sourceDeviceId === target.sourceDeviceId && notification.target.notificationKey === target.notificationKey && notification.target.lifetime === target.lifetime); if (item) item.dismissalPending = true; },
  dismiss_all_notifications: async () => { fixtureSnapshot.notifications.slice(0, 100).forEach(notification => { notification.dismissalPending = true; }); },
  set_app_muted: async (sourceDeviceId, packageName, appName, muted) => { const index = fixtureSnapshot.appFilters.findIndex(filter => filter.sourceDeviceId === sourceDeviceId && filter.packageName === packageName); const filter = { sourceDeviceId, packageName, appName, muted }; if (index < 0) fixtureSnapshot.appFilters.push(filter); else fixtureSnapshot.appFilters[index] = filter; },
  mark_notifications_seen: async targets => { fixtureSnapshot.notifications.forEach(notification => { if (targets.some(target => target.sourceDeviceId === notification.target.sourceDeviceId && target.notificationKey === notification.target.notificationKey && target.lifetime === notification.target.lifetime)) notification.seen = true; }); },
  set_notification_preferences: async preferences => { fixtureSnapshot.notificationPreferences = preferences; },
  set_notification_context: async () => undefined,
  request_notification_permission: async () => "granted",
  pick_attachments: async () => [{ id: `attachment-fixture-${Date.now()}`, name: "fixture-image.png", mediaType: "image/png", byteSize: 2400, state: "ready", previewUrl: "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==" }],
  retry_attachment: async () => undefined,
  save_attachment: async () => false,
  publish_attachment: async () => null,
  open_composer: async () => undefined,
  show_head: async () => undefined, update_head: async () => undefined, hide_head: async () => undefined, close_composer: async () => undefined,
  subscribe: () => () => undefined,
  window: async () => undefined,
};

const invoke = async <T>(command: string, args?: Record<string, unknown>) => (await import("@tauri-apps/api/core")).invoke<T>(command, args);
export const tauriBridge: DesktopBridge = {
  load_state: conversationId => invoke("load_state", { conversationId }), configure_server: origin => invoke("configure_server", { origin }), import_credentials: () => invoke("import_credentials"), unlock_sync: () => invoke("unlock_sync"), save_draft: input => invoke("save_draft", { input }), send_draft: input => invoke("send_draft", { input }), mark_seen: visibleMessageIds => invoke("mark_seen", { visibleMessageIds }), dismiss_notification: target => invoke("dismiss_notification", { target }), dismiss_all_notifications: () => invoke("dismiss_all_notifications"), set_app_muted: (sourceDeviceId, packageName, appName, muted) => invoke("set_app_muted", { sourceDeviceId, packageName, appName, muted }), mark_notifications_seen: targets => invoke("mark_notifications_seen", { targets }), set_notification_preferences: preferences => invoke("set_notification_preferences", { preferences }), set_notification_context: (view, conversationId) => invoke("set_notification_context", { view, conversationId }), request_notification_permission: () => invoke("request_notification_permission"), pick_attachments: () => invoke("pick_attachments"), retry_attachment: id => invoke("retry_attachment", { id }), save_attachment: id => invoke("save_attachment", { id }), publish_attachment: id => invoke("publish_attachment", { id }), open_composer: conversationId => invoke("open_composer", { conversationId }), show_head: conversationId => invoke("show_head", { conversationId }), update_head: conversationId => invoke("update_head", { conversationId }), hide_head: conversationId => invoke("hide_head", { conversationId }), close_composer: () => invoke("close_composer"), subscribe: listener => { let disposed = false; let unlisten: (() => void) | undefined; void import("@tauri-apps/api/event").then(({ listen }) => listen("openpush://state", () => listener())).then(stop => { unlisten = stop; if (disposed) stop(); }); return () => { disposed = true; unlisten?.(); }; },
  async window(action) { const w = (await import("@tauri-apps/api/window")).getCurrentWindow(); if (action === "minimize") await w.minimize(); else if (action === "maximize") await w.toggleMaximize(); else await w.close(); },
};

const missingHost = (method: keyof DesktopBridge) => async () => { throw { code: "missing-native-host", message: `Native host is required for ${String(method)}.` } satisfies BridgeError; };
export const missingHostBridge: DesktopBridge = Object.fromEntries((Object.keys(fixtureBridge) as (keyof DesktopBridge)[]).map(key => [key, key === "subscribe" ? (() => () => undefined) : missingHost(key)])) as unknown as DesktopBridge;
/** Fixtures are development/test-only and are never chosen after a native error. */
const environment = (import.meta as unknown as { env?: { DEV?: boolean; MODE?: string } }).env;
export const bridge: DesktopBridge = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window ? tauriBridge : (environment?.DEV || environment?.MODE === "test" ? fixtureBridge : missingHostBridge);
