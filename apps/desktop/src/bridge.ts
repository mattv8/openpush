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
export type DesktopSnapshot = { version: "1"; mode: "fixture" | "native"; connection: { state: ConnectionState; origin?: string; errorCode?: string }; encryption: { state: "locked" | "unlocked" | "preview" | "mismatch"; profileFingerprint?: string }; gateways: GatewayView[]; conversations: ConversationView[]; activeConversationId?: string; draft?: Draft; desktop?: { trayAvailable: boolean; startAtLogin: boolean; startupSupported: boolean; background: boolean }; head: { enabled: boolean; capability: "supported" | "unsupported" | "unconfirmed"; note?: string; panel?: boolean; pinnedConversationIds?: string[] }; pendingCount: number; quarantineCount: number; notifications: MirroredNotification[]; appFilters: AppFilter[]; notificationPreferences: NotificationPreferences; contactResolution?: ContactResolutionMap; contactBooks?: ContactBookView[]; contactsPendingCount?: number; contactSync?: ContactSyncStatus };
/** gatewayId/simId are optional on saves (omitted = keep the stored route) and required on sends. */
export type DraftInput = Pick<Draft, "id" | "conversationId" | "text" | "recipientIds" | "attachmentIds"> & { expectedRevision: string; gatewayId?: string; simId?: string };
export type SendDraftInput = DraftInput & { gatewayId: string; simId: string };
/** A separate, server-readable re-encoded copy created only after native confirmation. */
export type PublicCopy = { url: string; expiresInSeconds: number };
export type NotificationTarget = { sourceDeviceId: string; notificationKey: string; lifetime: string };
export type MirroredNotification = { target: NotificationTarget; packageName: string; appName: string; title: string; text: string; category?: string; postedAt: number; dismissible: boolean; seen: boolean; dismissalPending: boolean };
export type AppFilter = { sourceDeviceId: string; packageName: string; appName: string; muted: boolean };
export type NotificationPreferences = { messageBanners: boolean; mirroredBanners: boolean; preview: "full" | "hidden" };
export type ContactBookState = "active" | "limited" | "unavailable" | "retired";
/** Native values: requests go to the owner phone, so `canRestore` follows `canWrite`; `supportsNotes`/`supportsBirthday` follow the phone's platform (iOS has no notes). */
export type BookCapabilities = { canWrite: boolean; canDelete: boolean; supportsNotes: boolean; supportsPhoto: boolean; canRestore?: boolean; supportsBirthday?: boolean };
export type ContactBookView = { id: string; deviceName: string; state: ContactBookState; capabilities: BookCapabilities; contactCount: number; defaultAccountLabel?: string; lastSyncAt?: string; stalenessNote?: string; pendingEditCount: number };
/** `label` is the wire value (round-trips unchanged); `displayLabel` is for people (e.g. Apple raw labels). */
export type ContactPhone = { id?: string; label: string; displayLabel?: string; number: string; readOnly?: boolean };
export type ContactEmail = { id?: string; label: string; displayLabel?: string; address: string; readOnly?: boolean };
/** Unshown components (poBox, subLocality, isoCountryCode, …) must round-trip unchanged; `readOnly` items cannot be edited from this computer. */
export type ContactAddress = { id?: string; label: string; displayLabel?: string; formatted?: string; street?: string; poBox?: string; neighborhood?: string; subLocality?: string; city?: string; subAdministrativeArea?: string; state?: string; postalCode?: string; country?: string; isoCountryCode?: string; readOnly?: boolean };
export type ContactBirthday = { year?: number; month?: number; day?: number };
export type ContactView = { id: string; bookId: string; revision?: string; displayName: string; givenName?: string; familyName?: string; nickname?: string; phones: ContactPhone[]; emails: ContactEmail[]; addresses: ContactAddress[]; organization?: string; title?: string; birthday?: ContactBirthday; notes?: string; photoDataUrl?: string; photoPending?: boolean; pendingEditId?: string; pendingEditState?: "pending" | "awaiting-approval" | "outcome-unknown" | "conflict" | "rejected" | "failed" | "expired"; pendingEditSummary?: string; deletedAt?: string };
export type ContactEditRequestInput = { targetBookId: string; kind: "create" | "update" | "delete"; contactId?: string; baseRevision?: string; patches: unknown[]; photo: { kind: "keep" } | { kind: "remove" } | { kind: "set"; croppedDataUrl: string } };
export type ContactEditOutcome = { state: "pending"; requestId: string } | { state: "applied"; newRevision: string } | { state: "awaiting-approval" } | { state: "conflict"; conflictSummary: string } | { state: "rejected"; reason: string } | { state: "expired" };
/** Requester-ledger status of one edit request; only an owner result moves it past `pending`/`awaiting-approval`. */
export type ContactEditStatus = { requestId: string; bookId: string; state: "pending" | "awaiting-approval" | "outcome-unknown" | "applied" | "conflict" | "rejected" | "expired" | "failed"; reason?: string; kind?: "create" | "update" | "delete"; contactId?: string; displayName?: string; summary?: string; expiresAt?: number };
/** Contact projection health: a failed or pending rebuild means contacts may be stale. */
export type ContactSyncStatus = { repairRequired: boolean; projection?: { state: "current" | "rebuilding" | "failed"; reason?: string }; readiness?: { state: "server_unsupported" | "needs_unlock" | "backfill_pending" | "ready"; contacts_ready: boolean; server_active: boolean; backfill_unreadable: number } };
/** A phone number of a matching contact; `address` (never the contact ID) is the recipient. `normalized` is false for numbers kept as entered (e.g. short codes). */
export type RecipientSuggestion = { address: string; displayName: string; number: string; label?: string; normalized: boolean; contactId: string; phoneId: string; avatarUrl?: string };
export type RestorableContact = { id: string; bookId: string; displayName: string; deletedAt: string; photoDataUrl?: string };
export type ResolvedContact = { contactId: string; bookId: string; displayName: string; photoDataUrl?: string };
export type ContactResolutionMap = Record<string, ResolvedContact>;
/** `currentRevision` is set when the stored draft revision is known (stale saves/sends, refused sends). */
export type BridgeError = { code: string; message: string; currentRevision?: string };
/** Public pairing intent data only; credentials and challenge tokens stay native. */
export type PairingIntent = { httpsOrigin: string; intentToken: string; expiresInSeconds: number };
export type PairingStatus = { claimed: boolean; approved: boolean; deviceId?: string; keyDigest?: string; sas?: string; expiresInSeconds: number };

export interface DesktopBridge {
  load_state(conversationId?: string): Promise<DesktopSnapshot>;
  configure_server(origin: string): Promise<void>;
  import_credentials(): Promise<void>;
  unlock_sync(): Promise<void>;
  create_pairing_intent(): Promise<PairingIntent>;
  pairing_intent_status(intentToken: string): Promise<PairingStatus>;
  approve_pairing_intent(intentToken: string, keyDigest: string): Promise<void>;
  save_draft(input: DraftInput): Promise<Draft>;
  /** `accepted` means durably queued in the local encrypted outbox; `revision` is the stored draft revision afterwards. */
  send_draft(input: SendDraftInput): Promise<{ accepted: boolean; status: MessageStatus; reason?: string; revision?: string }>;
  mark_seen(visibleMessageIds: string[]): Promise<void>;
  dismiss_notification(target: NotificationTarget): Promise<void>;
  dismiss_all_notifications(): Promise<void>;
  set_app_muted(sourceDeviceId: string, packageName: string, appName: string, muted: boolean): Promise<void>;
  mark_notifications_seen(targets: NotificationTarget[]): Promise<void>;
  set_notification_preferences(preferences: NotificationPreferences): Promise<void>;
  set_notification_context(view: "conversations" | "notifications" | "settings" | "contacts", conversationId?: string): Promise<void>;
  request_notification_permission(): Promise<"granted" | "denied" | "unknown">;
  pick_attachments(): Promise<AttachmentView[]>;
  retry_attachment(id: string): Promise<void>;
  save_attachment(id: string): Promise<boolean>;
  /** Resolves null when the person cancels the native confirmation. */
  publish_attachment(id: string): Promise<PublicCopy | null>;
  /** Explicit user action only; omitting the ID starts a new-conversation draft. */
  open_composer(conversationId?: string): Promise<void>;
  set_start_at_login(enabled: boolean): Promise<void>;
  popout_conversation(conversationId: string): Promise<{ headCreated: boolean; warning?: string }>;
  hide_head(conversationId: string): Promise<void>;
  close_composer(): Promise<void>;
  close_head_panel(): Promise<void>;
  subscribe(listener: () => void): () => void;
  subscribe_lifecycle(listener: (request: { id: string; action: "quit" | "close" | "collapse" }) => void): () => void;
  subscribe_lifecycle_finished(listener: (result: { id: string; ok: boolean }) => void): () => void;
  acknowledge_lifecycle(id: string, ok: boolean): Promise<void>;
  window(action: "minimize" | "maximize" | "close"): Promise<void>;
  list_contact_books(): Promise<ContactBookView[]>;
  forget_contact_book(bookId: string): Promise<void>;
  /** One page of at most 200 contacts; request the next page with `offset`. */
  list_contacts(bookId: string, query?: string, offset?: number): Promise<ContactView[]>;
  submit_contact_edit(input: ContactEditRequestInput): Promise<ContactEditOutcome>;
  list_contact_edits(bookId?: string): Promise<ContactEditStatus[]>;
  search_contact_recipients(query: string, sourceDeviceId?: string): Promise<RecipientSuggestion[]>;
  request_contact_repair(): Promise<ContactSyncStatus>;
  pick_contact_photo(): Promise<{ dataUrl: string; naturalWidth: number; naturalHeight: number } | null>;
  list_restorable_contacts(bookId: string): Promise<RestorableContact[]>;
  restore_contact(bookId: string, contactId: string): Promise<ContactEditOutcome>;
}

const fixtureMessages: MessageView[] = [
  { id: "message-aurora-1", revision: "1", sender: "other", body: "Can you send over the estimate?", timestamp: "09:41", attachments: [] },
  { id: "message-aurora-2", revision: "2", sender: "self", body: "I'll have it to you shortly.", timestamp: "09:43", status: "sent", attachments: [] },
];
const fixtureSnapshot: DesktopSnapshot = { version: "1", mode: "fixture", connection: { state: "connected", origin: "https://push.example.com" }, encryption: { state: "unlocked" }, gateways: [{ id: "gateway-pixel8", name: "Pixel 8", simId: "sim-1", online: true, simulated: true, supportsSms: true, supportsMms: true, mmsContentVersion: 2, mmsMaxBytes: 300 * 1024, mmsLimitSource: "fallback", mmsMaxRecipients: 20 }], conversations: [{ id: "conv-aurora", name: "Aurora Chen", preview: "Can you send over the estimate?", unread: 2, messages: fixtureMessages }, { id: "conv-river", name: "River Park", preview: "Attachment received", unread: 0, participants: ["River Park", "Mina Torres"], messages: [{ id: "message-river-1", revision: "1", sender: "other", body: "Attachment received", timestamp: "Yesterday", transport: "mms", subject: "Estimate", attachments: [{ id: "attachment-river-1", name: "estimate.pdf", mediaType: "application/pdf", byteSize: 182000, state: "ready", transfer: "download", retryable: true }] }] }], activeConversationId: "conv-aurora", draft: undefined, head: { enabled: false, capability: "unsupported" }, pendingCount: 1, quarantineCount: 0, notifications: [{ target: { sourceDeviceId: "gateway-pixel8", notificationKey: "chat-42", lifetime: "1" }, packageName: "com.example.chat", appName: "Chat", title: "Morgan", text: "Are we still meeting after lunch?", postedAt: Date.now() - 120000, dismissible: true, seen: false, dismissalPending: false }, { target: { sourceDeviceId: "gateway-pixel8", notificationKey: "mail-7", lifetime: "1" }, packageName: "com.example.mail", appName: "Mail", title: "Project update", text: "A detailed update is ready for your review.", postedAt: Date.now() - 3600000, dismissible: true, seen: false, dismissalPending: false }], appFilters: [], notificationPreferences: { messageBanners: true, mirroredBanners: true, preview: "full" } };
// Exercise the same display-only contact resolution path as the native snapshot.
fixtureSnapshot.conversations[0].name = "+12025550123";
fixtureSnapshot.conversations[0].participants = ["+12025550123"];
fixtureSnapshot.contactResolution = {
  "+12025550123": { contactId: "contact-aurora", bookId: "book-pixel8", displayName: "Aurora Chen" },
};
fixtureSnapshot.contactsPendingCount = 1;

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
  create_pairing_intent: async () => ({ httpsOrigin: "https://push.example.com", intentToken: "A".repeat(43), expiresInSeconds: 300 }),
  pairing_intent_status: async () => ({ claimed: false, approved: false, expiresInSeconds: 300 }),
  approve_pairing_intent: async () => undefined,
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
  set_start_at_login: async enabled => { fixtureSnapshot.desktop = { trayAvailable: true, startAtLogin: enabled, startupSupported: true, background: false }; },
  popout_conversation: async conversationId => {
    if (!fixtureConversationIds().includes(conversationId)) throw fixtureError("not-found", "The requested conversation was not found.");
    fixtureSnapshot.head = { ...fixtureSnapshot.head, enabled: true, capability: "unconfirmed", pinnedConversationIds: [...new Set([...(fixtureSnapshot.head.pinnedConversationIds ?? []), conversationId])] };
    return { headCreated: true, warning: "Simulated fixture only; native floating input is not available." };
  },
  hide_head: async conversationId => { fixtureSnapshot.head = { ...fixtureSnapshot.head, pinnedConversationIds: fixtureSnapshot.head.pinnedConversationIds?.filter(id => id !== conversationId) }; }, close_composer: async () => undefined, close_head_panel: async () => undefined,
  subscribe: () => () => undefined,
  subscribe_lifecycle: () => () => undefined,
  subscribe_lifecycle_finished: () => () => undefined,
  acknowledge_lifecycle: async () => undefined,
  window: async () => undefined,
  list_contact_books: async () => [{ id: "book-pixel8", deviceName: "Pixel 8", state: "active", capabilities: { canWrite: true, canDelete: true, canRestore: true, supportsNotes: true, supportsPhoto: true, supportsBirthday: true }, contactCount: 3, defaultAccountLabel: "Google · aurora@example.com", lastSyncAt: "Just now", pendingEditCount: 1 }, { id: "book-iphone-retired", deviceName: "iPhone", state: "retired", capabilities: { canWrite: false, canDelete: false, supportsNotes: false, supportsPhoto: true }, contactCount: 0, lastSyncAt: "3 days ago", pendingEditCount: 0 }],
  forget_contact_book: async () => undefined,
  list_contacts: async (bookId, query) => [{ id: "contact-aurora", bookId, revision: "1", displayName: "Aurora Chen", givenName: "Aurora", familyName: "Chen", phones: [{ label: "mobile", number: "+12025550123" }], emails: [{ label: "work", address: "aurora@example.com" }], addresses: [], organization: "Northstar", title: "Director", notes: "Prefers SMS", pendingEditId: "edit-aurora", pendingEditState: "pending" as const, pendingEditSummary: "Phone number update" }, { id: "contact-river", bookId, revision: "1", displayName: "River Park", givenName: "River", familyName: "Park", phones: [{ label: "mobile", number: "+12025550124" }], emails: [], addresses: [] }, { id: "contact-mina", bookId, revision: "1", displayName: "Mina Torres", givenName: "Mina", familyName: "Torres", phones: [{ label: "work", number: "+12025550125" }], emails: [], addresses: [] }].filter(contact => !query || `${contact.displayName} ${contact.phones.map(phone => phone.number).join(" ")}`.toLowerCase().includes(query.toLowerCase())),
  submit_contact_edit: async () => ({ state: "pending", requestId: `fixture-${Date.now()}` }),
  list_contact_edits: async bookId => [{ requestId: "edit-aurora", bookId: bookId ?? "book-pixel8", state: "pending" }],
  search_contact_recipients: async query => {
    const needle = query.trim().toLowerCase();
    if (!needle) return [];
    const people = [
      { contactId: "contact-aurora", name: "Aurora Chen", phones: [{ id: "p-aurora", label: "mobile", number: "+12025550123" }] },
      { contactId: "contact-sol", name: "Sol Rivera", phones: [{ id: "p-sol-1", label: "mobile", number: "+12025550160" }, { id: "p-sol-2", label: "work", number: "+12025550161" }] },
    ];
    const digits = needle.replace(/\D/g, "");
    return people.flatMap(person => person.phones
      .filter(phone => person.name.toLowerCase().includes(needle) || (digits.length >= 2 && phone.number.includes(digits)))
      .map(phone => ({ address: phone.number, displayName: person.name, number: phone.number, label: phone.label, normalized: true, contactId: person.contactId, phoneId: phone.id })));
  },
  request_contact_repair: async () => ({ repairRequired: true }),
  pick_contact_photo: async () => ({ dataUrl: "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==", naturalWidth: 1, naturalHeight: 1 }),
  list_restorable_contacts: async bookId => [{ id: "contact-deleted", bookId, displayName: "Casey Rowan", deletedAt: "Yesterday" }],
  restore_contact: async () => ({ state: "pending", requestId: `fixture-restore-${Date.now()}` }),
};

const invoke = async <T>(command: string, args?: Record<string, unknown>) => (await import("@tauri-apps/api/core")).invoke<T>(command, args);
export const tauriBridge: DesktopBridge = {
  load_state: conversationId => invoke("load_state", { conversationId }), configure_server: origin => invoke("configure_server", { origin }), import_credentials: () => invoke("import_credentials"), unlock_sync: () => invoke("unlock_sync"), create_pairing_intent: () => invoke("create_pairing_intent"), pairing_intent_status: intentToken => invoke("pairing_intent_status", { intentToken }), approve_pairing_intent: (intentToken, keyDigest) => invoke("approve_pairing_intent", { intentToken, keyDigest }), save_draft: input => invoke("save_draft", { input }), send_draft: input => invoke("send_draft", { input }), mark_seen: visibleMessageIds => invoke("mark_seen", { visibleMessageIds }), dismiss_notification: target => invoke("dismiss_notification", { target }), dismiss_all_notifications: () => invoke("dismiss_all_notifications"), set_app_muted: (sourceDeviceId, packageName, appName, muted) => invoke("set_app_muted", { sourceDeviceId, packageName, appName, muted }), mark_notifications_seen: targets => invoke("mark_notifications_seen", { targets }), set_notification_preferences: preferences => invoke("set_notification_preferences", { preferences }), set_notification_context: (view, conversationId) => invoke("set_notification_context", { view, conversationId }), request_notification_permission: () => invoke("request_notification_permission"), pick_attachments: () => invoke("pick_attachments"), retry_attachment: id => invoke("retry_attachment", { id }), save_attachment: id => invoke("save_attachment", { id }), publish_attachment: id => invoke("publish_attachment", { id }), open_composer: conversationId => invoke("open_composer", { conversationId }), set_start_at_login: enabled => invoke("set_start_at_login", { enabled }), popout_conversation: conversationId => invoke("popout_conversation", { conversationId }), hide_head: conversationId => invoke("hide_head", { conversationId }), close_composer: () => invoke("close_composer"), close_head_panel: () => invoke("close_head_panel"), subscribe: listener => subscribeEvent("peppy://state", () => listener()), subscribe_lifecycle: listener => subscribeWindowEvent("peppy://lifecycle-request", (payload: unknown) => { if (isLifecycleRequest(payload)) listener(payload); }), subscribe_lifecycle_finished: listener => subscribeWindowEvent("peppy://lifecycle-finished", (payload: unknown) => { if (isLifecycleFinished(payload)) listener(payload); }), acknowledge_lifecycle: (id, ok) => invoke("acknowledge_lifecycle", { id, ok }),
  list_contact_books: () => invoke("list_contact_books"), forget_contact_book: bookId => invoke("forget_contact_book", { bookId }), list_contacts: (bookId, query, offset) => invoke("list_contacts", { bookId, query, offset }), submit_contact_edit: input => invoke("submit_contact_edit", { input }), list_contact_edits: bookId => invoke("list_contact_edits", { bookId }), search_contact_recipients: (query, sourceDeviceId) => invoke("search_contact_recipients", { query, sourceDeviceId }), request_contact_repair: () => invoke("request_contact_repair"), pick_contact_photo: () => invoke("pick_contact_photo"), list_restorable_contacts: bookId => invoke("list_restorable_contacts", { bookId }), restore_contact: (bookId, contactId) => invoke("restore_contact", { bookId, contactId }),
  async window(action) { const w = (await import("@tauri-apps/api/window")).getCurrentWindow(); if (action === "minimize") await w.minimize(); else if (action === "maximize") await w.toggleMaximize(); else await w.close(); },
};

const isLifecycleRequest = (value: unknown): value is { id: string; action: "quit" | "close" | "collapse" } => typeof value === "object" && value !== null && typeof (value as { id?: unknown }).id === "string" && ["quit", "close", "collapse"].includes((value as { action?: unknown }).action as string);
const isLifecycleFinished = (value: unknown): value is { id: string; ok: boolean } => typeof value === "object" && value !== null && typeof (value as { id?: unknown }).id === "string" && typeof (value as { ok?: unknown }).ok === "boolean";
const subscribeEvent = (event: string, listener: (payload: unknown) => void) => {
  let disposed = false;
  let unlisten: (() => void) | undefined;
  void import("@tauri-apps/api/event").then(({ listen }) => listen(event, payload => listener(payload.payload))).then(stop => { unlisten = stop; if (disposed) stop(); }).catch(() => {});
  return () => { disposed = true; unlisten?.(); };
};
const subscribeWindowEvent = (event: string, listener: (payload: unknown) => void) => {
  let disposed = false;
  let unlisten: (() => void) | undefined;
  void import("@tauri-apps/api/webviewWindow").then(({ getCurrentWebviewWindow }) => getCurrentWebviewWindow().listen(event, payload => listener(payload.payload))).then(stop => { unlisten = stop; if (disposed) stop(); }).catch(() => {});
  return () => { disposed = true; unlisten?.(); };
};
const missingHost = (method: keyof DesktopBridge) => async () => { throw { code: "missing-native-host", message: `Native host is required for ${String(method)}.` } satisfies BridgeError; };
export const missingHostBridge: DesktopBridge = Object.fromEntries((Object.keys(fixtureBridge) as (keyof DesktopBridge)[]).map(key => [key, key === "subscribe" || key === "subscribe_lifecycle" || key === "subscribe_lifecycle_finished" ? (() => () => undefined) : missingHost(key)])) as unknown as DesktopBridge;
/** Fixtures are development/test-only and are never chosen after a native error. */
const environment = (import.meta as unknown as { env?: { DEV?: boolean; MODE?: string } }).env;
export const bridge: DesktopBridge = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window ? tauriBridge : (environment?.DEV || environment?.MODE === "test" ? fixtureBridge : missingHostBridge);
