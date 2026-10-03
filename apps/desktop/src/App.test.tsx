import "@testing-library/jest-dom/vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App, DraftStore } from "./App";
import {
  bridge,
  fixtureBridge,
  type ConversationView,
  type DesktopSnapshot,
  type Draft,
  type DraftInput,
  type GatewayView,
  type SendDraftInput,
} from "./bridge";

/**
 * In-memory stand-in for the native host contract (apps/desktop/src-tauri/src/session.rs):
 * an empty draft ID asks the host to create the draft (and, with an empty conversation ID, a new
 * conversation); saves are CAS on `expectedRevision`; sends use the STORED draft and clear it.
 */
type HostError = { code: string; message: string };
const SMS_ONLY: GatewayView = {
  id: "gw-phone",
  name: "Phone",
  simId: "sim-1",
  online: false,
  simulated: false,
  supportsSms: true,
  supportsMms: false,
};
describe("draft lifecycle flushing", () => {
  it("attempts every dirty slot and reports the first failed save", async () => {
    const persisted: string[] = [];
    const store = new DraftStore(async input => {
      persisted.push(input.conversationId);
      if (input.conversationId === "one") throw { message: "first failed" };
      return { id: `draft-${input.conversationId}`, conversationId: input.conversationId, text: input.text, recipientIds: input.recipientIds, attachmentIds: input.attachmentIds, revision: "1" };
    }, { changed: () => {}, rekeyed: () => {} });
    void store.edit("one", { text: "a" });
    void store.edit("two", { text: "b" });
    expect(await store.flushAll()).toEqual({ ok: false, error: "first failed" });
    expect(persisted).toEqual(expect.arrayContaining(["one", "two"]));
  });

  it("serializes concurrent flushing behind queued revisions", async () => {
    const first = deferred<Draft>();
    const inputs: DraftInput[] = [];
    let active = 0;
    let maximumActive = 0;
    const store = new DraftStore(async input => {
      inputs.push(input);
      active += 1;
      maximumActive = Math.max(maximumActive, active);
      const saved = inputs.length === 1 ? await first.promise : { ...input, id: "draft-one", revision: "2" };
      active -= 1;
      return saved;
    }, { changed: () => {}, rekeyed: () => {} });
    const firstEdit = store.edit("one", { text: "a" });
    await vi.waitFor(() => expect(inputs).toHaveLength(1));
    const secondEdit = store.edit("one", { text: "b" });
    const flush = store.flushAll();
    first.resolve({ id: "draft-one", conversationId: "one", text: "a", recipientIds: [], attachmentIds: [], revision: "1" });
    await Promise.all([firstEdit, secondEdit, flush]);
    expect(inputs).toMatchObject([{ text: "a", expectedRevision: "0" }, { text: "b", expectedRevision: "1" }]);
    expect(maximumActive).toBe(1);
  });
});

const MMS_SIM: GatewayView = { ...SMS_ONLY, simId: "sim-2", supportsMms: true, mmsContentVersion: 2 };

function createHost() {
  let created = 0;
  const host = {
    gateways: [SMS_ONLY, MMS_SIM] as GatewayView[],
    connection: {
      state: "connected",
      origin: "https://example.test",
    } as DesktopSnapshot["connection"],
    encryption: { state: "unlocked" } as DesktopSnapshot["encryption"],
    desktop: { trayAvailable: true, startAtLogin: false, startupSupported: true, background: false } as NonNullable<DesktopSnapshot["desktop"]>,
    head: { enabled: false, capability: "unsupported" } as DesktopSnapshot["head"],
    conversations: [
      {
        id: "aurora",
        name: "Aurora",
        preview: "Hello",
        unread: 1,
        messages: [
          {
            id: "m-aurora",
            revision: "1",
            sender: "other",
            body: "Hello from Aurora",
            timestamp: "now",
            attachments: [
              {
                id: "photo-1",
                name: "photo.png",
                mediaType: "image/png",
                byteSize: 10,
                state: "ready",
                transfer: "download",
                retryable: true,
              },
            ],
          },
        ],
      },
      {
        id: "river",
        name: "River",
        preview: "Hi",
        unread: 0,
        messages: [
          {
            id: "m-river",
            revision: "1",
            sender: "other",
            body: "Hello from River",
            timestamp: "now",
            attachments: [],
          },
        ],
      },
    ] as ConversationView[],
    drafts: new Map<string, Draft>(),
    failSaves: undefined as HostError | undefined,
    sent: [] as Draft[],
    load(conversationId?: string): DesktopSnapshot {
      const known = [
        ...host.conversations.map((c) => c.id),
        ...host.drafts.keys(),
      ];
      const active =
        conversationId && known.includes(conversationId)
          ? conversationId
          : known[0];
      const listed = host.conversations.map((c) => ({
        ...c,
        messages: c.id === active ? c.messages : [],
      }));
      const draftOnly = [...host.drafts.values()]
        .filter(
          (d) => !host.conversations.some((c) => c.id === d.conversationId),
        )
        .map((d) => ({
          id: d.conversationId,
          name: d.recipientIds.join(", ") || "New message",
          preview: "Draft",
          unread: 0,
          messages: [],
        }));
      return {
        version: "1",
        mode: "native",
        connection: host.connection,
        encryption: host.encryption,
        gateways: host.gateways,
        conversations: [...listed, ...draftOnly],
        activeConversationId: active,
        draft: active ? host.drafts.get(active) : undefined,
        desktop: host.desktop,
        head: host.head,
        pendingCount: 0,
        quarantineCount: 0,
        notifications: [],
        appFilters: [],
        notificationPreferences: {
          messageBanners: true,
          mirroredBanners: true,
          preview: "full",
        },
      };
    },
    save(input: DraftInput): Draft {
      if (host.failSaves) throw host.failSaves;
      let current: Draft | undefined;
      if (input.id === "") {
        const conversationId = input.conversationId || `conv-new-${++created}`;
        current = host.drafts.get(conversationId) ?? {
          id: `draft-${conversationId}`,
          conversationId,
          text: "",
          recipientIds: [],
          attachmentIds: [],
          revision: "0",
        };
      } else {
        current = [...host.drafts.values()].find((d) => d.id === input.id);
        if (!current)
          throw {
            code: "not-found",
            message: "The requested stored item was not found.",
          } satisfies HostError;
      }
      if (
        input.conversationId &&
        input.conversationId !== current.conversationId
      )
        throw {
          code: "invalid-draft",
          message: "The draft does not belong to this conversation.",
        } satisfies HostError;
      if (current.revision !== input.expectedRevision)
        throw {
          code: "stale-draft",
          message: `The draft changed elsewhere (current revision ${current.revision}); both versions were kept.`,
        } satisfies HostError;
      const saved: Draft = {
        ...current,
        text: input.text,
        recipientIds: input.recipientIds,
        attachmentIds: input.attachmentIds,
        gatewayId: input.gatewayId ?? current.gatewayId,
        simId: input.simId ?? current.simId,
        revision: String(Number(current.revision) + 1),
      };
      host.drafts.set(saved.conversationId, saved);
      return saved;
    },
    send(input: SendDraftInput) {
      const stored = [...host.drafts.values()].find((d) => d.id === input.id);
      if (!stored)
        throw {
          code: "not-found",
          message: "The requested stored item was not found.",
        } satisfies HostError;
      if (stored.revision !== input.expectedRevision)
        throw {
          code: "stale-draft",
          message: "The draft changed elsewhere; both versions were kept.",
        } satisfies HostError;
      host.drafts.delete(stored.conversationId);
      host.sent.push({
        ...stored,
        gatewayId: input.gatewayId,
        simId: input.simId,
      });
      return { accepted: true, status: "queued-local" as const };
    },
  };
  return host;
}

let host: ReturnType<typeof createHost>;
let hint: (() => void) | undefined;
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
};
const message = () => screen.getByLabelText("Message") as HTMLTextAreaElement;
const type = (value: string) =>
  fireEvent.change(message(), { target: { value } });
const routeValue = (gateway: GatewayView) =>
  `${encodeURIComponent(gateway.id)} ${encodeURIComponent(gateway.simId)}`;
const openComposerWindow = (conversationId: string, head = false) =>
  window.history.replaceState(
    {},
    "",
    `/?window=composer&conversationId=${conversationId}${head ? "&head=1" : ""}`,
  );
let lifecycle: ((request: { id: string; action: "quit" | "close" | "collapse" }) => void) | undefined;
let lifecycleDispose: ReturnType<typeof vi.fn>;
let lifecycleFinished: ((result: { id: string; ok: boolean }) => void) | undefined;
let lifecycleFinishedDispose: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.restoreAllMocks();
  host = createHost();
  hint = undefined;
  lifecycle = undefined;
  lifecycleDispose = vi.fn();
  lifecycleFinished = undefined;
  lifecycleFinishedDispose = vi.fn();
  vi.spyOn(bridge, "load_state").mockImplementation(async (id) =>
    host.load(id),
  );
  vi.spyOn(bridge, "save_draft").mockImplementation(async (input) =>
    host.save(input),
  );
  vi.spyOn(bridge, "send_draft").mockImplementation(async (input) =>
    host.send(input),
  );
  vi.spyOn(bridge, "pick_attachments").mockResolvedValue([
    {
      id: "file-1",
      name: "file.png",
      mediaType: "image/png",
      byteSize: 2,
      state: "ready",
    },
  ]);
  vi.spyOn(bridge, "retry_attachment").mockResolvedValue();
  vi.spyOn(bridge, "save_attachment").mockResolvedValue(true);
  vi.spyOn(bridge, "mark_seen").mockResolvedValue();
  vi.spyOn(bridge, "publish_attachment").mockResolvedValue(null);
  vi.spyOn(bridge, "open_composer").mockResolvedValue();
  vi.spyOn(bridge, "close_composer").mockResolvedValue();
  vi.spyOn(bridge, "close_head_panel").mockResolvedValue();
  vi.spyOn(bridge, "popout_conversation").mockResolvedValue({ headCreated: true });
  vi.spyOn(bridge, "set_start_at_login").mockResolvedValue();
  vi.spyOn(bridge, "acknowledge_lifecycle").mockResolvedValue();
  vi.spyOn(bridge, "subscribe_lifecycle").mockImplementation(listener => {
    lifecycle = listener;
    return lifecycleDispose;
  });
  vi.spyOn(bridge, "subscribe_lifecycle_finished").mockImplementation(listener => {
    lifecycleFinished = listener;
    return lifecycleFinishedDispose;
  });
  vi.spyOn(bridge, "subscribe").mockImplementation((listener) => {
    hint = listener;
    return () => undefined;
  });
});
afterEach(() => {
  cleanup();
  localStorage.removeItem("openpush.layout.v1");
  window.history.replaceState({}, "", "/");
  vi.unstubAllGlobals();
});

describe("new-recipient drafts", () => {
  it("creates the draft with empty IDs, then saves and sends with the host-assigned IDs", async () => {
    const save = vi.mocked(bridge.save_draft);
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });

    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0][0]).toMatchObject({
      id: "",
      conversationId: "",
      recipientIds: ["+12025550100"],
      expectedRevision: "0",
    });
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"),
    );
    expect(document.querySelector('[data-recipient-id="+12025550100"]')).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove (202) 555-0100" })).toBeInTheDocument();
    expect(screen.getByLabelText("Recipients")).toHaveValue("");

    type("first message");
    await waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save.mock.calls[1][0]).toMatchObject({
      id: "draft-conv-new-1",
      conversationId: "conv-new-1",
      text: "first message",
      expectedRevision: "1",
    });

    fireEvent.change(screen.getByLabelText("Gateway"), {
      target: { value: routeValue(SMS_ONLY) },
    });
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.simId).toBe("sim-1"),
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(host.sent).toHaveLength(1));
    expect(host.sent[0]).toMatchObject({
      conversationId: "conv-new-1",
      text: "first message",
      recipientIds: ["+12025550100"],
      gatewayId: "gw-phone",
      simId: "sim-1",
    });
    expect(
      save.mock.calls.every(
        ([input]) => input.id === "" || input.id === "draft-conv-new-1",
      ),
    ).toBe(true);
  });

  it("keeps a rejected new recipient as an unsaved local draft that can be corrected", async () => {
    render(<App />);
    host.failSaves = {
      code: "invalid-recipient",
      message: "Recipients must be phone numbers.",
    };
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Recipients must be phone numbers.",
    );
    expect(document.querySelector('[data-recipient-id="+12025550100"]')).toBeInTheDocument();

    host.failSaves = undefined;
    fireEvent.click(screen.getByRole("button", { name: "Remove (202) 555-0100" }));
    fireEvent.change(screen.getByLabelText("Recipients"), {
      target: { value: "+1 202 555 0101" },
    });
    fireEvent.blur(screen.getByLabelText("Recipients"));
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([
        "+12025550101",
      ]),
    );
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });

  it("does not save an invalid new recipient", async () => {
    const save = vi.mocked(bridge.save_draft);
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "alice@example" } });
    fireEvent.keyDown(picker, { key: "Enter" });

    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();
    expect(picker).toHaveValue("alice@example");
  });

  it("blocks Send while a recipient is pending, then saves the canonical number on commit", async () => {
    host.gateways = [MMS_SIM];
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() => expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual(["+12025550100"]));
    type("ready to send");
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());

    const recipients = screen.getByLabelText("Recipients");
    fireEvent.change(recipients, { target: { value: "+1 202" } });
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(document.getElementById("unavailable-hint")).toHaveTextContent(
      "finish adding the recipient",
    );

    fireEvent.change(recipients, { target: { value: "+1 202 555 0101" } });
    await waitFor(() => expect(recipients).toHaveValue("+1 202 555 0101"));
    fireEvent.keyDown(recipients, { key: "Enter" });
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([
        "+12025550100",
        "+12025550101",
      ]),
    );
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  });

  it("unblocks Send when an incoming message replaces a pending recipient panel", async () => {
    host.gateways = [MMS_SIM];
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() => expect(host.drafts.get("conv-new-1")).toBeDefined());

    type("reply after incoming message");
    const recipients = screen.getByLabelText("Recipients");
    fireEvent.change(recipients, { target: { value: "+1 202" } });
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();

    host.conversations.push({
      id: "conv-new-1",
      name: "+12025550100",
      preview: "Incoming while composing",
      unread: 1,
      messages: [{
        id: "m-conv-new-1",
        revision: "1",
        sender: "other",
        body: "Incoming while composing",
        timestamp: "now",
        attachments: [],
      }],
    });
    await act(async () => hint?.());

    await waitFor(() =>
      expect(screen.queryByRole("region", { name: "Message recipients" })).not.toBeInTheDocument(),
    );
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  });

  it("persists an empty recipient list after removing the last chip without restoring it on blur", async () => {
    host.gateways = [MMS_SIM];
    const save = vi.mocked(bridge.save_draft);
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() => expect(host.drafts.get("conv-new-1")).toBeDefined());

    type("ready to send");
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
    const recipients = screen.getByLabelText("Recipients");
    fireEvent.change(recipients, { target: { value: "+1 202" } });
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Remove (202) 555-0100" }));
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([]),
    );
    fireEvent.blur(recipients);
    expect(document.querySelector('[data-recipient-id="+12025550100"]')).not.toBeInTheDocument();
    expect(recipients).toHaveValue("+1 202");
    expect(save.mock.calls.some(([input]) => input.recipientIds.length === 0)).toBe(true);
  });

  it("removes the exact stored ID from a multi-recipient draft", async () => {
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() =>
      expect(document.querySelector('[data-conversation-id="conv-new-1"]')).toBeInTheDocument(),
    );
    const recipients = screen.getByLabelText("Recipients");
    fireEvent.change(recipients, { target: { value: "+1 202 555 0101" } });
    await waitFor(() => expect(recipients).toHaveValue("+1 202 555 0101"));
    fireEvent.keyDown(recipients, { key: "Enter" });
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([
        "+12025550100",
        "+12025550101",
      ]),
    );

    fireEvent.click(screen.getByRole("button", { name: "Remove (202) 555-0100" }));
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([
        "+12025550101",
      ]),
    );
    expect(document.querySelector('[data-recipient-id="+12025550100"]')).not.toBeInTheDocument();
    expect(document.querySelector('[data-recipient-id="+12025550101"]')).toBeInTheDocument();
  });
});

describe("floating conversation actions", () => {
  it("opens the row target without changing selection", async () => {
    render(<App />);
    await waitFor(() => expect(document.querySelector('[data-header-title]')).toHaveTextContent("Aurora"));
    fireEvent.click(document.querySelector('[data-popout-conversation-id="river"]')!);
    await waitFor(() => expect(bridge.popout_conversation).toHaveBeenCalledWith("river"));
    expect(document.querySelector('[data-header-title]')).toHaveTextContent("Aurora");
  });

  it("blocks popout when saving the target draft fails", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    host.failSaves = { code: "io", message: "Disk full." };
    type("unsaved target");
    await screen.findByRole("alert");
    fireEvent.click(document.getElementById("header-popout-conversation")!);
    await screen.findByText(/Not opened/);
    expect(bridge.popout_conversation).not.toHaveBeenCalled();
  });

  it("uses the persisted conversation ID after a local draft is rekeyed", async () => {
    render(<App />);
    const picker = await screen.findByRole("combobox", { name: "Search recipients" });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() => expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"));
    fireEvent.click(document.getElementById("header-popout-conversation")!);
    await waitFor(() => expect(bridge.popout_conversation).toHaveBeenCalledWith("conv-new-1"));
  });
});

describe("conversation selection", () => {
  it("loads the selected conversation's messages and stored draft from the host", async () => {
    host.drafts.set("river", {
      id: "draft-river",
      conversationId: "river",
      text: "river draft",
      recipientIds: ["+12025550100"],
      attachmentIds: [],
      revision: "4",
    });
    render(<App />);
    expect(await screen.findByText("Hello from Aurora")).toBeInTheDocument();
    type("aurora text");
    fireEvent.click(screen.getByRole("button", { name: /River/ }));
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("river"),
    );
    expect(await screen.findByText("Hello from River")).toBeInTheDocument();
    expect(screen.queryByText("Hello from Aurora")).not.toBeInTheDocument();
    await waitFor(() => expect(message()).toHaveValue("river draft"));
    await waitFor(() =>
      expect(host.drafts.get("aurora")?.text).toBe("aurora text"),
    );

    type("river edit");
    await waitFor(() =>
      expect(host.drafts.get("river")).toMatchObject({
        text: "river edit",
        revision: "5",
      }),
    );
  });

  it("ignores a slower response for a conversation that is no longer selected", async () => {
    const slow = deferred<DesktopSnapshot>();
    render(<App />);
    await screen.findByText("Hello from Aurora");
    vi.mocked(bridge.load_state).mockImplementationOnce(() => slow.promise);
    fireEvent.click(screen.getByRole("button", { name: /River/ }));
    fireEvent.click(screen.getByRole("button", { name: /Aurora/ }));
    expect(await screen.findByText("Hello from Aurora")).toBeInTheDocument();
    await act(async () => slow.resolve(host.load("river")));
    expect(screen.getByText("Hello from Aurora")).toBeInTheDocument();
  });
});

describe("draft durability", () => {
  it("serializes delayed saves against the acknowledged revision without replacing newer text", async () => {
    const first = deferred<Draft>();
    const save = vi
      .mocked(bridge.save_draft)
      .mockImplementationOnce(() => first.promise);
    render(<App />);
    await screen.findByText("Hello from Aurora");
    type("first");
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    type("newer");
    first.resolve(host.save(save.mock.calls[0][0]));
    await waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save.mock.calls[1][0]).toMatchObject({
      id: "draft-aurora",
      text: "newer",
      expectedRevision: "1",
    });
    await waitFor(() => expect(host.drafts.get("aurora")?.text).toBe("newer"));
    expect(message()).toHaveValue("newer");
  });

  it("blocks send after a rejected save, keeps the edits, and sends the latest text once a retry succeeds", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.change(screen.getByLabelText("Gateway"), {
      target: { value: routeValue(SMS_ONLY) },
    });
    await waitFor(() => expect(host.drafts.get("aurora")?.simId).toBe("sim-1"));
    type("saved text");
    await waitFor(() =>
      expect(host.drafts.get("aurora")?.text).toBe("saved text"),
    );

    host.failSaves = { code: "io", message: "Disk full." };
    type("latest text");
    expect(await screen.findByRole("alert")).toHaveTextContent("Disk full.");
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(
      await screen.findByText(/Not sent: the draft could not be saved/),
    ).toBeInTheDocument();
    expect(bridge.send_draft).not.toHaveBeenCalled();
    expect(message()).toHaveValue("latest text");

    host.failSaves = undefined;
    fireEvent.click(screen.getByRole("button", { name: "Retry save" }));
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(host.sent).toHaveLength(1));
    expect(host.sent[0].text).toBe("latest text");
    await waitFor(() => expect(message()).toHaveValue(""));
  });

  it("recovers from a stale-revision conflict by keeping this window's text over the stored revision", async () => {
    host.drafts.set("aurora", {
      id: "draft-aurora",
      conversationId: "aurora",
      text: "stored",
      recipientIds: [],
      attachmentIds: [],
      revision: "1",
    });
    render(<App />);
    await waitFor(() => expect(message()).toHaveValue("stored"));
    host.drafts.set("aurora", {
      ...host.drafts.get("aurora")!,
      text: "other window",
      revision: "2",
    });
    type("mine");
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "changed elsewhere",
    );
    fireEvent.click(screen.getByRole("button", { name: "Retry save" }));
    await waitFor(() =>
      expect(host.drafts.get("aurora")).toMatchObject({
        text: "mine",
        revision: "3",
      }),
    );
  });

  it("retains dirty drafts across live state hints and adopts host changes once clean", async () => {
    const pending = deferred<Draft>();
    const save = vi
      .mocked(bridge.save_draft)
      .mockImplementationOnce(() => pending.promise);
    render(<App />);
    await screen.findByText("Hello from Aurora");
    type("keep local");
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    host.drafts.set("aurora", {
      id: "draft-aurora",
      conversationId: "aurora",
      text: "host text",
      recipientIds: [],
      attachmentIds: [],
      revision: "7",
    });
    await act(async () => hint?.());
    await waitFor(() => expect(bridge.load_state).toHaveBeenCalledTimes(2));
    expect(message()).toHaveValue("keep local");

    pending.resolve({
      id: "draft-aurora",
      conversationId: "aurora",
      text: "keep local",
      recipientIds: [],
      attachmentIds: [],
      revision: "8",
    });
    host.drafts.set("aurora", {
      id: "draft-aurora",
      conversationId: "aurora",
      text: "keep local",
      recipientIds: [],
      attachmentIds: [],
      revision: "8",
    });
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
    host.drafts.set("aurora", {
      id: "draft-aurora",
      conversationId: "aurora",
      text: "edited in composer window",
      recipientIds: [],
      attachmentIds: [],
      revision: "9",
    });
    await act(async () => hint?.());
    await waitFor(() =>
      expect(message()).toHaveValue("edited in composer window"),
    );
  });

  it("keeps a draft whose save failed when a live hint arrives", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    host.failSaves = { code: "io", message: "Disk full." };
    type("unsaved");
    await screen.findByRole("alert");
    await act(async () => hint?.());
    await waitFor(() => expect(bridge.load_state).toHaveBeenCalledTimes(2));
    expect(message()).toHaveValue("unsaved");
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });
});

describe("composer window", () => {
  it("bootstraps head chrome and converts in place without losing the draft", async () => {
    openComposerWindow("aurora", true);
    render(<App />);
    expect(await screen.findByRole("banner", { name: "Floating conversation with Aurora" })).toBeInTheDocument();
    type("preserve while converting");
    await waitFor(() => expect(host.drafts.get("aurora")?.text).toBe("preserve while converting"));
    host.head = { ...host.head, panel: false };
    await act(async () => hint?.());
    await waitFor(() => expect(document.getElementById("desktop-titlebar")).toBeInTheDocument());
    expect(message()).toHaveValue("preserve while converting");
  });
  it("closes a head panel only after its draft flushes", async () => {
    openComposerWindow("aurora", true);
    render(<App />);
    await screen.findByRole("banner", { name: "Floating conversation with Aurora" });
    type("text to save");
    await waitFor(() => expect(bridge.save_draft).toHaveBeenCalled());
    const saveDraftCalls = vi.mocked(bridge.save_draft).mock.calls.length;
    const saveOrder = vi.mocked(bridge.save_draft).mock.invocationCallOrder[saveDraftCalls - 1];
    fireEvent.click(screen.getByRole("button", { name: "Close bubble" }));
    await waitFor(() => expect(bridge.close_head_panel).toHaveBeenCalledOnce());
    const closeOrder = vi.mocked(bridge.close_head_panel).mock.invocationCallOrder[0];
    expect(saveOrder).toBeLessThan(closeOrder);
    expect(bridge.close_composer).not.toHaveBeenCalled();
  });

  it("marks the close button busy until the native close finishes", async () => {
    const pending = deferred<void>();
    vi.mocked(bridge.close_head_panel).mockReturnValueOnce(pending.promise);
    openComposerWindow("aurora", true);
    render(<App />);
    const close = await screen.findByRole("button", { name: "Close bubble" });
    fireEvent.click(close);
    expect(await screen.findByRole("button", { name: "Closing…" })).toHaveAttribute("aria-busy", "true");
    pending.resolve();
    await waitFor(() => expect(screen.getByRole("button", { name: "Close bubble" })).toBeEnabled());
  });

  it("keeps a head panel open when its draft flush fails", async () => {
    openComposerWindow("aurora", true);
    render(<App />);
    await screen.findByRole("banner", { name: "Floating conversation with Aurora" });
    host.failSaves = { code: "io", message: "Disk full." };
    type("do not lose me");
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Close bubble" }));
    expect(await screen.findByText(/window stayed open/)).toBeInTheDocument();
    expect(bridge.close_head_panel).not.toHaveBeenCalled();
    expect(screen.getByRole("banner", { name: "Floating conversation with Aurora" })).toBeInTheDocument();
  });

  it("collapses a head panel without closing its bubble, including on Escape", async () => {
    openComposerWindow("aurora", true);
    render(<App />);
    await screen.findByRole("banner", { name: "Floating conversation with Aurora" });
    fireEvent.click(screen.getByRole("button", { name: "Collapse to bubble" }));
    await waitFor(() => expect(bridge.close_composer).toHaveBeenCalledOnce());
    expect(bridge.close_head_panel).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(bridge.close_composer).toHaveBeenCalledTimes(2));
  });

  it("uses and persists the separate head composer height", async () => {
    localStorage.setItem("openpush.layout.v1", JSON.stringify({ composerHeight: 128, headComposerHeight: 160 }));
    openComposerWindow("aurora", true);
    render(<App />);
    const grip = await screen.findByRole("separator", { name: "Resize composer" });
    expect(grip).toHaveAttribute("aria-valuenow", "160");
    fireEvent.keyDown(grip, { key: "ArrowDown" });
    await waitFor(() => {
      const layout = JSON.parse(localStorage.getItem("openpush.layout.v1")!);
      expect(layout.composerHeight).toBe(128);
      expect(layout.headComposerHeight).not.toBe(160);
    });
  });

  it("re-applies a saved head composer height when a standalone composer becomes a panel", async () => {
    // JSDOM reports zero layout heights, which would clamp every value to the minimum.
    vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(800);
    localStorage.setItem("openpush.layout.v1", JSON.stringify({ composerHeight: 128, headComposerHeight: 112 }));
    openComposerWindow("aurora");
    render(<App />);
    expect((await screen.findByRole("separator", { name: "Resize composer" }))).toHaveAttribute("aria-valuenow", "128");
    host.head = { ...host.head, panel: true };
    await act(async () => hint?.());
    await waitFor(() => expect(document.getElementById("head-panel-header")).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("separator", { name: "Resize composer" })).toHaveAttribute("aria-valuenow", "112"));
    expect(JSON.parse(localStorage.getItem("openpush.layout.v1")!).composerHeight).toBe(128);
  });
  it("loads the conversation named by the native URL and closes only after the draft is saved", async () => {
    openComposerWindow("aurora");
    render(<App />);
    await screen.findByText("Hello from Aurora");
    expect(bridge.load_state).toHaveBeenCalledWith("aurora");
    expect(screen.getByRole("banner")).toHaveAttribute(
      "data-tauri-drag-region",
    );
    expect(document.querySelector("#desktop-titlebar [data-disclosure=\"sync-state\"]")).toBeInTheDocument();
    expect(screen.queryByLabelText("Server URL")).not.toBeInTheDocument();

    host.failSaves = { code: "io", message: "Disk full." };
    type("do not lose me");
    await screen.findByRole("alert");
    fireEvent.keyDown(window, { key: "Escape" });
    expect(await screen.findByText(/window stayed open/)).toBeInTheDocument();
    fireEvent.click(screen.getAllByRole("button", { name: "Close composer" })[0]);
    await waitFor(() =>
      expect(
        vi.mocked(bridge.save_draft).mock.calls.length,
      ).toBeGreaterThanOrEqual(3),
    );
    expect(bridge.close_composer).not.toHaveBeenCalled();
    expect(message()).toHaveValue("do not lose me");

    host.failSaves = undefined;
    fireEvent.keyDown(window, { key: "Escape", isComposing: true });
    await Promise.resolve();
    expect(bridge.close_composer).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(bridge.close_composer).toHaveBeenCalledTimes(1));
    expect(host.drafts.get("aurora")?.text).toBe("do not lose me");
  });

  it("offers a recipient field for a host-created draft-only conversation", async () => {
    host.drafts.set("conv-tray", {
      id: "draft-tray",
      conversationId: "conv-tray",
      text: "",
      recipientIds: [],
      attachmentIds: [],
      revision: "0",
    });
    openComposerWindow("conv-tray");
    render(<App />);
    const recipients = await screen.findByLabelText("Recipients");
    fireEvent.change(recipients, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(recipients, { key: "Enter" });
    await waitFor(() =>
      expect(host.drafts.get("conv-tray")).toMatchObject({
        id: "draft-tray",
        recipientIds: ["+12025550100"],
        revision: "1",
      }),
    );
  });

  it("uses floating heads for conversations and standalone composer for new messages", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.click(document.getElementById("header-popout-conversation")!);
    await waitFor(() => expect(bridge.popout_conversation).toHaveBeenCalledWith("aurora"));
    expect(bridge.open_composer).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "New message window" }));
    await waitFor(() => expect(bridge.open_composer).toHaveBeenLastCalledWith(undefined));
  });
});

describe("native lifecycle requests", () => {
  it("acks clean actions and disposes its listener", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    for (const action of ["quit", "close", "collapse"] as const)
      await act(async () => lifecycle?.({ id: `request-${action}`, action }));
    await waitFor(() => expect(bridge.acknowledge_lifecycle).toHaveBeenCalledTimes(3));
    cleanup();
    expect(lifecycleDispose).toHaveBeenCalledOnce();
  });

  it("freezes edits after acknowledgement until the targeted lifecycle finishes", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    await act(async () => lifecycle?.({ id: "request-switch", action: "close" }));
    await waitFor(() => expect(bridge.acknowledge_lifecycle).toHaveBeenCalledWith("request-switch", true));
    expect(document.getElementById("desktop-shell")).toHaveAttribute("inert");
    await act(async () => lifecycleFinished?.({ id: "other", ok: false }));
    expect(document.getElementById("desktop-shell")).toHaveAttribute("inert");
    await act(async () => lifecycleFinished?.({ id: "request-switch", ok: false }));
    expect(document.getElementById("desktop-shell")).not.toHaveAttribute("inert");
    cleanup();
    expect(lifecycleFinishedDispose).toHaveBeenCalledOnce();
  });

  it("attempts every dirty store and nacks when any save fails", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    host.failSaves = { code: "io", message: "Disk full." };
    type("unsaved aurora");
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: /River/ }));
    await screen.findByText("Hello from River");
    type("unsaved river");
    await waitFor(() => expect(message()).toHaveValue("unsaved river"));
    vi.mocked(bridge.save_draft).mockClear();
    await act(async () => lifecycle?.({ id: "request-quit", action: "quit" }));
    await waitFor(() => expect(bridge.acknowledge_lifecycle).toHaveBeenCalledWith("request-quit", false));
    expect(vi.mocked(bridge.save_draft).mock.calls.map(([input]) => input.conversationId)).toEqual(expect.arrayContaining(["aurora", "river"]));
  });
});

describe("gateway routes", () => {
  const storedDraft = (overrides: Partial<Draft> = {}): Draft => ({
    id: "draft-aurora",
    conversationId: "aurora",
    text: "MMS reply",
    recipientIds: [],
    attachmentIds: [],
    gatewayId: "gw-phone",
    simId: "sim-2",
    revision: "1",
    ...overrides,
  });

  it("requires a version-2-or-newer MMS route for text-only groups and keeps the draft", async () => {
    host.gateways = [{ ...MMS_SIM, mmsContentVersion: undefined }];
    host.drafts.set("aurora", storedDraft({ recipientIds: ["+15550001", "+15550002"] }));
    render(<App />);
    await waitFor(() => expect(message()).toHaveValue("MMS reply"));
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(screen.getByText(/needs MMS content version 2/)).toBeInTheDocument();
    expect(host.drafts.get("aurora")?.text).toBe("MMS reply");
  });

  it("accepts a newer MMS content version for a text-only group", async () => {
    host.gateways = [{ ...MMS_SIM, mmsContentVersion: 3 }];
    host.drafts.set("aurora", storedDraft({ recipientIds: ["+15550001", "+15550002"] }));
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(bridge.send_draft).toHaveBeenCalled());
  });

  it("keeps text-only replies to an MMS conversation on an MMS route", async () => {
    host.gateways = [SMS_ONLY];
    host.conversations[0].messages[0].transport = "mms";
    host.drafts.set("aurora", storedDraft({ gatewayId: "gw-phone", simId: "sim-1" }));
    render(<App />);
    await waitFor(() => expect(screen.getByText(/does not support MMS attachments/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("does not promote a current SMS reply because of an older picture message", async () => {
    host.gateways = [SMS_ONLY];
    const conversation = host.conversations[0];
    conversation.messages[0].transport = "mms";
    conversation.messages.push({
      ...conversation.messages[0], id: "latest-sms", revision: "2",
      transport: "sms", body: "Back to text", attachments: [],
    });
    host.drafts.set("aurora", storedDraft({ gatewayId: "gw-phone", simId: "sim-1" }));
    render(<App />);
    await waitFor(() => expect(message()).toHaveValue("MMS reply"));
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  });

  it("blocks replies with the latest reason while preserving the draft", async () => {
    host.gateways = [{ ...MMS_SIM }];
    host.conversations[0].replyBlockedReason = "confirm your address on the phone";
    host.drafts.set("aurora", storedDraft());
    render(<App />);
    await waitFor(() => expect(message()).toHaveValue("MMS reply"));
    expect(screen.getByText(/Replies unavailable: confirm your address/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(bridge.send_draft).not.toHaveBeenCalled();
    expect(host.drafts.get("aurora")?.text).toBe("MMS reply");
  });

  it("removes a draft attachment through the CAS save path", async () => {
    host.drafts.set("aurora", storedDraft({ attachmentIds: ["file-1"] }));
    render(<App />);
    await screen.findByRole("button", { name: "Remove Attached file" });
    fireEvent.click(screen.getByRole("button", { name: "Remove Attached file" }));
    await waitFor(() => expect(host.drafts.get("aurora")?.attachmentIds).toEqual([]));
    expect(vi.mocked(bridge.save_draft).mock.calls.at(-1)?.[0]).toMatchObject({ attachmentIds: [] });
  });

  it("retries and saves the exact attachment ID without sending", async () => {
    render(<App />);
    await screen.findByRole("button", { name: "Retry download for photo.png" });
    fireEvent.click(screen.getByRole("button", { name: "Retry download for photo.png" }));
    fireEvent.click(screen.getByRole("button", { name: "Save photo.png" }));
    await waitFor(() => expect(bridge.retry_attachment).toHaveBeenCalledWith("photo-1"));
    expect(bridge.save_attachment).toHaveBeenCalledWith("photo-1");
    expect(bridge.send_draft).not.toHaveBeenCalled();
  });

  it("blocks an MMS lower-bound estimate that already exceeds the reported limit", async () => {
    host.gateways = [{ ...MMS_SIM, mmsMaxBytes: 1, mmsLimitSource: "carrier" }];
    host.drafts.set("aurora", storedDraft({ attachmentIds: ["file-1"] }));
    render(<App />);
    await screen.findByText(/estimate exceeds/);
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("rechecks the current stored conversation after a flush updates its eligibility", async () => {
    host.gateways = [{ ...MMS_SIM }];
    host.drafts.set("aurora", storedDraft({ text: "before flush" }));
    const pending = deferred<Draft>();
    vi.mocked(bridge.save_draft).mockImplementationOnce(async (input) => pending.promise.then(() => host.save(input)));
    render(<App />);
    await screen.findByDisplayValue("before flush");
    fireEvent.change(message(), { target: { value: "after flush" } });
    await waitFor(() => expect(bridge.save_draft).toHaveBeenCalled());
    host.conversations[0].replyBlockedReason = "confirm your address on the phone";
    await act(async () => hint?.());
    pending.resolve(host.drafts.get("aurora")!);
    await waitFor(() => expect(screen.getByText(/Replies unavailable: confirm your address/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(bridge.send_draft).not.toHaveBeenCalled();
  });

  it("distinguishes two SIMs on one gateway device and blocks MMS on the SMS-only SIM", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    const select = screen.getByLabelText("Gateway") as HTMLSelectElement;
    const values = within(select)
      .getAllByRole("option")
      .map((option) => (option as HTMLOptionElement).value);
    expect(new Set(values.filter(Boolean)).size).toBe(2);
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: "Add attachment" }));
    await waitFor(() =>
      expect(host.drafts.get("aurora")?.attachmentIds).toEqual(["file-1"]),
    );
    fireEvent.change(select, { target: { value: routeValue(SMS_ONLY) } });
    await waitFor(() =>
      expect(host.drafts.get("aurora")).toMatchObject({
        gatewayId: "gw-phone",
        simId: "sim-1",
      }),
    );
    expect(
      screen.getByText(/does not support MMS attachments/),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();

    fireEvent.change(select, { target: { value: routeValue(MMS_SIM) } });
    await waitFor(() =>
      expect(host.drafts.get("aurora")).toMatchObject({
        gatewayId: "gw-phone",
        simId: "sim-2",
      }),
    );
    expect(document.getElementById("unavailable-hint")).toHaveAttribute(
      "hidden",
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(bridge.send_draft).toHaveBeenCalledWith(
        expect.objectContaining({
          gatewayId: "gw-phone",
          simId: "sim-2",
          attachmentIds: ["file-1"],
        }),
      ),
    );
  });

  it("never reroutes a stored route that is no longer reported", async () => {
    host.gateways = [SMS_ONLY];
    host.drafts.set("aurora", {
      id: "draft-aurora",
      conversationId: "aurora",
      text: "hello",
      recipientIds: [],
      attachmentIds: [],
      gatewayId: "gw-phone",
      simId: "sim-9",
      revision: "2",
    });
    render(<App />);
    await waitFor(() => expect(message()).toHaveValue("hello"));
    expect(screen.getByLabelText("Gateway")).toHaveDisplayValue(
      "Unavailable route · SIM sim-9",
    );
    expect(
      screen.getByText(/no longer reported/),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.keyDown(message(), { key: "Enter" });
    await Promise.resolve();
    expect(bridge.send_draft).not.toHaveBeenCalled();
    expect(bridge.save_draft).not.toHaveBeenCalled();
  });
});

describe("host state display", () => {
  it("formats numeric conversation labels while preserving existing names", async () => {
    host.conversations[1].name = "+12025550100";
    render(<App />);
    expect(await screen.findByRole("button", { name: /Aurora/ })).toBeInTheDocument();
    const numericConversation = screen.getByRole("button", {
      name: /\(202\) 555-0100/,
    });
    fireEvent.click(numericConversation);
    await waitFor(() =>
      expect(document.querySelector("[data-header-title]")).toHaveTextContent(
        "(202) 555-0100",
      ),
    );
    expect(screen.getByRole("button", { name: /Aurora/ })).toBeInTheDocument();
  });

  it("shows guided onboarding when disconnected with no selection and invokes native setup actions", async () => {
    host.conversations = [];
    host.connection = {
      state: "offline",
      origin: "",
      errorCode: "server-required",
    };
    host.encryption = { state: "locked" };
    const configure = vi.spyOn(bridge, "configure_server").mockResolvedValue();
    render(<App />);
    const onboarding = await screen.findByRole("region", {
      name: "Set up OpenPush",
    });
    expect(onboarding).toBeInTheDocument();
    expect(within(onboarding).getAllByRole("listitem")).toHaveLength(3);
    fireEvent.change(screen.getByLabelText("Server URL"), {
      target: { value: "https://server.test" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Configure server" }));
    await waitFor(() =>
      expect(configure).toHaveBeenCalledWith("https://server.test"),
    );
  });

  it("replaces both panes with notifications and preserves the draft when returning", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), { target: { value: "Keep this draft" } });
    fireEvent.click(screen.getByRole("button", { name: /^Notifications/ }));
    expect(screen.getByRole("region", { name: "Notifications" })).toBeInTheDocument();
    expect(document.getElementById("thread-list")).not.toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    expect(document.getElementById("notification-settings")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Conversations" }));
    expect(document.getElementById("thread-list")).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveValue("Keep this draft");
  });

  it("switches to settings from the rail and applies the selected theme class", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(
      screen.getByRole("region", { name: "Settings" }),
    ).toBeInTheDocument();
    expect(document.getElementById("connection-status")).toHaveAttribute(
      "data-connection-state",
      "connected",
    );
    expect(
      screen.getByText("Carrier SMS/MMS not end-to-end encrypted"),
    ).toBeInTheDocument();
    expect(document.getElementById("thread-list")).not.toBeVisible();
    expect(
      screen.queryByRole("separator", { name: "Resize thread list" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "New message window" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("heading", { level: 1, name: "Settings" }),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Theme"), {
      target: { value: "dark" },
    });
    expect(document.getElementById("desktop-shell")).toHaveClass("theme-dark");
    fireEvent.click(screen.getByRole("button", { name: "Conversations" }));
    expect(document.getElementById("thread-list")).toBeVisible();
    expect(
      screen.queryByRole("region", { name: "Settings" }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Hello from Aurora")).toBeInTheDocument();
  });

  it("rolls back and re-enables start at login after a failed change", async () => {
    const pending = deferred<void>();
    vi.mocked(bridge.set_start_at_login).mockImplementation(() => pending.promise);
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    const startup = screen.getByRole("checkbox", { name: "Start at login" });
    fireEvent.click(startup);
    expect(startup).toBeDisabled();
    expect(startup).not.toBeChecked();
    pending.reject({ message: "Registration denied." });
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    await waitFor(() => expect(startup).toBeEnabled());
    expect(startup).not.toBeChecked();
  });

  it("shows the public URL returned by publish_attachment and reports cancellation", async () => {
    vi.mocked(bridge.publish_attachment)
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce({
        url: "https://example.test/file/mms-usercontent/token/photo.png",
        expiresInSeconds: 7200,
      });
    render(<App />);
    await screen.findByText("Hello from Aurora");
    fireEvent.click(
      screen.getByRole("button", { name: "Create public link for photo.png" }),
    );
    expect(
      await screen.findByText(/Public link not created/),
    ).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: "Create public link for photo.png" }),
    );
    expect(
      await screen.findByLabelText("Public link for photo.png"),
    ).toHaveValue("https://example.test/file/mms-usercontent/token/photo.png");
    expect(screen.getByText("Expires in 2 h.")).toBeInTheDocument();
    expect(bridge.publish_attachment).toHaveBeenCalledWith("photo-1");
  });

  it("shows the native connection error code", async () => {
    host.connection = {
      state: "connected",
      origin: "https://example.test",
      errorCode: "outbox-rejected",
    };
    render(<App />);
    expect(
      await screen.findByText(/the server rejected queued messages/),
    ).toBeInTheDocument();
    host.connection = {
      state: "offline",
      origin: "https://example.test",
      errorCode: "live-x",
    };
    await act(async () => hint?.());
    expect(await screen.findByText("Offline — live-x")).toBeInTheDocument();
  });

  it("renders global status pills in the titlebar", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    const titlebar = document.getElementById("desktop-titlebar")!;
    expect(titlebar.querySelector("#connection-status")).toBeInTheDocument();
    expect(titlebar.querySelector("[data-disclosure=\"carrier-sms\"]")).toBeInTheDocument();
    expect(document.querySelector("#conversation-pane #connection-status")).not.toBeInTheDocument();
    expect(document.querySelector("#conversation-pane [data-disclosure=\"carrier-sms\"]")).not.toBeInTheDocument();
  });

  it("does not repeat a connection state used as its error code", async () => {
    host.connection = {
      state: "offline",
      origin: "https://example.test",
      errorCode: "offline",
    };
    render(<App />);
    expect(await screen.findByText("Offline")).toBeInTheDocument();
    expect(screen.queryByText("Offline — offline")).not.toBeInTheDocument();
  });

  it("retries a rejected visible-row mark after native state refresh", async () => {
    const mark = vi.mocked(bridge.mark_seen);
    mark.mockRejectedValueOnce({ message: "Window is hidden." });
    vi.spyOn(document, "hasFocus").mockReturnValue(true);
    const observed: ((
      entries: { target: Element; isIntersecting: boolean }[],
    ) => void)[] = [];
    const targets: Element[] = [];
    class Observer {
      constructor(
        private callback: (
          entries: { target: Element; isIntersecting: boolean }[],
        ) => void,
      ) {
        observed.push(callback);
      }
      observe(target: Element) {
        targets.push(target);
        this.callback([{ target, isIntersecting: false }]);
      }
      disconnect() {}
      unobserve() {}
      takeRecords() {
        return [];
      }
      root = null;
      rootMargin = "";
      thresholds = [];
    }
    vi.stubGlobal("IntersectionObserver", Observer);
    render(<App />);
    await screen.findByText("Hello from Aurora");
    expect(mark).not.toHaveBeenCalled();
    const row = targets.find(
      (target) => target.getAttribute("data-message-id") === "m-aurora",
    )!;
    act(() => observed.at(-1)!([{ target: row, isIntersecting: true }]));
    await waitFor(() => expect(mark).toHaveBeenCalledTimes(1));
    await screen.findByText("Window is hidden.");
    await act(async () => hint?.());
    await waitFor(() => expect(mark).toHaveBeenCalledTimes(2));
    expect(mark).toHaveBeenLastCalledWith(["m-aurora"]);
  });
});

describe("desktop presentation controls", () => {
  it("shows the overlay scrollbar while the message list is scrolled", async () => {
    render(<App />);
    const list = await screen.findByRole("log", { name: "Messages" });
    await waitFor(() =>
      expect(list).not.toHaveAttribute("data-scroll-programmatic"),
    );
    fireEvent.scroll(list);
    expect(list).toHaveAttribute("data-scrolling", "true");
  });

  it("scrolls the message list to the bottom when a conversation opens", async () => {
    const scrollHeight = Object.getOwnPropertyDescriptor(
      HTMLElement.prototype,
      "scrollHeight",
    );
    const clientHeight = Object.getOwnPropertyDescriptor(
      HTMLElement.prototype,
      "clientHeight",
    );
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get: () => 400,
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get: () => 100,
    });
    try {
      render(<App />);
      const list = await screen.findByRole("log", { name: "Messages" });
      expect(list.scrollTop).toBe(400);
    } finally {
      if (scrollHeight)
        Object.defineProperty(HTMLElement.prototype, "scrollHeight", scrollHeight);
      else delete (HTMLElement.prototype as { scrollHeight?: number }).scrollHeight;
      if (clientHeight)
        Object.defineProperty(HTMLElement.prototype, "clientHeight", clientHeight);
      else delete (HTMLElement.prototype as { clientHeight?: number }).clientHeight;
    }
  });

  it("renders a narrow list without persisting its window clamp", async () => {
    localStorage.setItem(
      "openpush.layout.v1",
      JSON.stringify({ listWidth: 480, listCollapsed: false }),
    );
    const innerWidth = Object.getOwnPropertyDescriptor(window, "innerWidth");
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 760,
    });
    try {
      render(<App />);
      await screen.findByText("Hello from Aurora");
      fireEvent.resize(window);
      const list = document.getElementById("thread-list")!;
      expect(Number.parseFloat(list.style.width)).toBeLessThanOrEqual(351);
      fireEvent.keyDown(
        screen.getByRole("separator", { name: "Resize composer" }),
        { key: "ArrowDown" },
      );
      expect(JSON.parse(localStorage.getItem("openpush.layout.v1")!)).toMatchObject({
        listWidth: 480,
      });
    } finally {
      if (innerWidth) Object.defineProperty(window, "innerWidth", innerWidth);
    }
  });

  it("falls back safely from corrupt persisted layout", async () => {
    localStorage.setItem("openpush.layout.v1", "not-json");
    expect(() => render(<App />)).not.toThrow();
    await screen.findByText("Hello from Aurora");
    expect(document.getElementById("thread-list")).toBeVisible();
  });

  it("clamps a too-small saved composer height without persisting the display clamp", async () => {
    localStorage.setItem(
      "openpush.layout.v1",
      JSON.stringify({ listWidth: 280, listCollapsed: false, composerHeight: 72 }),
    );
    render(<App />);
    await screen.findByText("Hello from Aurora");
    expect(document.getElementById("handle-h2")).toHaveAttribute(
      "aria-valuenow",
      "96",
    );
    expect(localStorage.getItem("openpush.layout.v1")).toContain(
      '"composerHeight":72',
    );
  });

  it("puts the composer resize grip on the floating card over the message list", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    const stage = document.getElementById("conversation-stage")!;
    const area = document.getElementById("composer-area")!;
    expect(stage).toContainElement(screen.getByRole("log", { name: "Messages" }));
    expect(stage).toContainElement(area);
    expect(area).toContainElement(
      screen.getByRole("separator", { name: "Resize composer" }),
    );
  });

  it("renders the recipient panel in the safe rail layer between messages and composer", async () => {
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"),
    );
    const panel = screen.getByRole("region", { name: "Message recipients" });
    const stage = document.getElementById("conversation-stage")!;
    const layer = document.getElementById("recipient-rail-layer")!;
    const composer = document.getElementById("composer-area")!;
    expect(layer.parentElement).toBe(stage);
    expect(panel.parentElement).toBe(layer);
    expect(layer.previousElementSibling).toBe(screen.getByRole("log", { name: "Messages" }));
    expect(layer.nextElementSibling).toBe(composer);
    expect(composer).not.toContainElement(panel);
  });

  it("restores a valid persisted recipient panel position", async () => {
    localStorage.setItem(
      "openpush.layout.v1",
      JSON.stringify({ listWidth: 320, recipientPosition: { x: 37, y: 53 } }),
    );
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"),
    );
    const panel = screen.getByRole("region", { name: "Message recipients" });
    expect(panel).toHaveStyle({ left: "37px", top: "53px" });
    const savedLayout = localStorage.getItem("openpush.layout.v1");
    fireEvent.resize(window);
    expect(localStorage.getItem("openpush.layout.v1")).toBe(savedLayout);
  });

  it("defaults invalid recipient panel positions and preserves layout fields on keyboard movement", async () => {
    localStorage.setItem(
      "openpush.layout.v1",
      JSON.stringify({ listWidth: 320, recipientAnchor: "bottom-left", sibling: true, recipientPosition: { x: -1, y: 53 } }),
    );
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    fireEvent.change(picker, { target: { value: "+1 202 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"),
    );
    const panel = screen.getByRole("region", { name: "Message recipients" });
    expect(panel.style.left).not.toBe("-1px");
    fireEvent.keyDown(
      screen.getByRole("button", { name: "Move recipients panel" }),
      { key: "ArrowDown" },
    );
    await waitFor(() => {
      expect(JSON.parse(localStorage.getItem("openpush.layout.v1")!)).toMatchObject({
        listWidth: 320,
        sibling: true,
        recipientPosition: expect.objectContaining({ x: expect.any(Number), y: expect.any(Number) }),
      });
      expect(JSON.parse(localStorage.getItem("openpush.layout.v1")!)).not.toHaveProperty("recipientAnchor");
    });
  });

  it("renders the recipient panel in the safe rail layer in a composer window", async () => {
    // jsdom does not supply PointerEvent coordinates without a constructor.
    vi.stubGlobal("PointerEvent", MouseEvent);
    openComposerWindow("aurora");
    host.conversations[0].messages = [];
    render(<App />);
    const panel = await screen.findByRole("region", { name: "Message recipients" });
    const stage = document.getElementById("conversation-stage")!;
    const layer = document.getElementById("recipient-rail-layer")!;
    expect(layer.parentElement).toBe(stage);
    expect(panel.parentElement).toBe(layer);
    expect(layer.previousElementSibling).toBe(screen.getByRole("log", { name: "Messages" }));
    expect(layer.nextElementSibling).toBe(document.getElementById("composer-area"));
    const grip = screen.getByRole("button", { name: "Move recipients panel" });
    const before = { left: panel.style.left, top: panel.style.top };
    const saved = localStorage.getItem("openpush.layout.v1");
    fireEvent.pointerDown(grip, {
      pointerId: 1,
      clientX: 10,
      clientY: 10,
    });
    fireEvent.pointerMove(grip, { pointerId: 1, clientX: 80, clientY: 60 });
    expect(document.querySelector("[data-recipient-anchor], #recipient-drop-targets, .recipient-drop-target")).not.toBeInTheDocument();
    fireEvent.pointerCancel(grip, { pointerId: 1 });
    expect(panel.style.left).toBe(before.left);
    expect(panel.style.top).toBe(before.top);
    expect(localStorage.getItem("openpush.layout.v1")).toBe(saved);
  });

  it("collapses and restores the thread list with Enter and persists it", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    const handle = document.getElementById("handle-h1")!;
    fireEvent.keyDown(handle, { key: "Enter" });
    expect(document.getElementById("thread-list")).toHaveAttribute(
      "data-collapsed",
      "true",
    );
    expect(localStorage.getItem("openpush.layout.v1")).toContain(
      '"listCollapsed":true',
    );
    fireEvent.keyDown(handle, { key: "Enter" });
    expect(document.getElementById("thread-list")).not.toHaveAttribute(
      "data-collapsed",
    );
  });

  it("uses the Conversations rail button to toggle the list", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    const conversations = screen.getByRole("button", { name: "Conversations" });
    expect(conversations).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(conversations);
    expect(conversations).toHaveAttribute("aria-expanded", "false");
    expect(document.getElementById("thread-list")).toHaveAttribute(
      "data-collapsed",
      "true",
    );
  });
});

describe("development fixture bridge", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("assigns stable draft and conversation IDs for an empty-ID save and enforces revisions like the host", async () => {
    const created = await fixtureBridge.save_draft({
      id: "",
      conversationId: "",
      text: "",
      recipientIds: ["+12025550100"],
      attachmentIds: [],
      expectedRevision: "0",
    });
    expect(created.id).not.toBe("");
    expect(created.conversationId).not.toBe("");
    expect(created).toMatchObject({
      recipientIds: ["+12025550100"],
      revision: "1",
    });

    const updated = await fixtureBridge.save_draft({
      id: created.id,
      conversationId: created.conversationId,
      text: "hi",
      recipientIds: [],
      attachmentIds: [],
      expectedRevision: "1",
    });
    expect(updated).toMatchObject({
      id: created.id,
      conversationId: created.conversationId,
      text: "hi",
      recipientIds: [],
      revision: "2",
    });
    await expect(
      fixtureBridge.save_draft({
        id: created.id,
        conversationId: created.conversationId,
        text: "late",
        recipientIds: [],
        attachmentIds: [],
        expectedRevision: "1",
      }),
    ).rejects.toMatchObject({ code: "stale-draft" });
    await expect(
      fixtureBridge.save_draft({
        id: "draft-unknown",
        conversationId: "",
        text: "",
        recipientIds: [],
        attachmentIds: [],
        expectedRevision: "0",
      }),
    ).rejects.toMatchObject({ code: "not-found" });

    const reattached = await fixtureBridge.save_draft({
      id: "",
      conversationId: created.conversationId,
      text: "again",
      recipientIds: [],
      attachmentIds: [],
      expectedRevision: "2",
    });
    expect(reattached.id).toBe(created.id);

    const state = await fixtureBridge.load_state(created.conversationId);
    expect(state.mode).toBe("fixture");
    expect(state.activeConversationId).toBe(created.conversationId);
    expect(state.draft).toMatchObject({
      id: created.id,
      text: "again",
      revision: "3",
    });
    expect(
      state.conversations.some(
        (conversation) => conversation.id === created.conversationId,
      ),
    ).toBe(true);
  });

  it("runs the new-recipient flow in fixture mode without debug labels", async () => {
    const save = vi.spyOn(fixtureBridge, "save_draft");
    render(<App />);
    const picker = await screen.findByRole("combobox", {
      name: "Search recipients",
    });
    for (const debugText of [
      "SIMULATED UI",
      "DEVELOPMENT FIXTURE",
      "Browser fixture",
      "sim-fixture",
    ])
      expect(document.body.textContent).not.toContain(debugText);
    fireEvent.change(picker, { target: { value: "+1 202 555 0199" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    const assigned = await save.mock.results[0].value;
    expect(assigned.conversationId).not.toBe("");
    type("fixture text");
    await waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save.mock.calls[1][0]).toMatchObject({
      id: assigned.id,
      conversationId: assigned.conversationId,
      expectedRevision: "1",
      text: "fixture text",
    });
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
    expect(
      within(document.getElementById("thread-list")!).getByRole("button", {
        name: /\(202\) 555-0199/,
      }),
    ).toHaveAttribute("data-conversation-id", assigned.conversationId);
  });
});
