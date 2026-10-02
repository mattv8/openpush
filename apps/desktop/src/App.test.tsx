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
import { App } from "./App";
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
const MMS_SIM: GatewayView = { ...SMS_ONLY, simId: "sim-2", supportsMms: true };

function createHost() {
  let created = 0;
  const host = {
    gateways: [SMS_ONLY, MMS_SIM] as GatewayView[],
    connection: {
      state: "connected",
      origin: "https://example.test",
    } as DesktopSnapshot["connection"],
    encryption: { state: "unlocked" } as DesktopSnapshot["encryption"],
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
        head: { enabled: false, capability: "unsupported" },
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
        recipientIds: input.recipientIds.length
          ? input.recipientIds
          : current.recipientIds,
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
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
};
const message = () => screen.getByLabelText("Message") as HTMLTextAreaElement;
const type = (value: string) =>
  fireEvent.change(message(), { target: { value } });
const routeValue = (gateway: GatewayView) =>
  `${encodeURIComponent(gateway.id)} ${encodeURIComponent(gateway.simId)}`;
const openComposerWindow = (conversationId: string) =>
  window.history.replaceState(
    {},
    "",
    `/?window=composer&conversationId=${conversationId}`,
  );

beforeEach(() => {
  vi.restoreAllMocks();
  host = createHost();
  hint = undefined;
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
  vi.spyOn(bridge, "mark_seen").mockResolvedValue();
  vi.spyOn(bridge, "publish_attachment").mockResolvedValue(null);
  vi.spyOn(bridge, "open_composer").mockResolvedValue();
  vi.spyOn(bridge, "close_composer").mockResolvedValue();
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
    fireEvent.change(picker, { target: { value: "+1 555 0100" } });
    fireEvent.keyDown(picker, { key: "Enter" });

    await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0][0]).toMatchObject({
      id: "",
      conversationId: "",
      recipientIds: ["+1 555 0100"],
      expectedRevision: "0",
    });
    await waitFor(() =>
      expect(bridge.load_state).toHaveBeenLastCalledWith("conv-new-1"),
    );
    expect(screen.getByLabelText("Recipients")).toHaveValue("+1 555 0100");

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
      recipientIds: ["+1 555 0100"],
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
    fireEvent.change(picker, { target: { value: "alice@example" } });
    fireEvent.keyDown(picker, { key: "Enter" });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Recipients must be phone numbers.",
    );
    expect(
      screen.getByRole("button", { name: /alice@example/ }),
    ).toBeInTheDocument();

    host.failSaves = undefined;
    fireEvent.change(screen.getByLabelText("Recipients"), {
      target: { value: "+15550100" },
    });
    fireEvent.blur(screen.getByLabelText("Recipients"));
    await waitFor(() =>
      expect(host.drafts.get("conv-new-1")?.recipientIds).toEqual([
        "+15550100",
      ]),
    );
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });
});

describe("conversation selection", () => {
  it("loads the selected conversation's messages and stored draft from the host", async () => {
    host.drafts.set("river", {
      id: "draft-river",
      conversationId: "river",
      text: "river draft",
      recipientIds: [],
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
  it("loads the conversation named by the native URL and closes only after the draft is saved", async () => {
    openComposerWindow("aurora");
    render(<App />);
    await screen.findByText("Hello from Aurora");
    expect(bridge.load_state).toHaveBeenCalledWith("aurora");
    expect(screen.getByRole("banner")).toHaveAttribute(
      "data-tauri-drag-region",
    );
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
    fireEvent.change(recipients, { target: { value: "+15550100" } });
    fireEvent.keyDown(recipients, { key: "Enter" });
    await waitFor(() =>
      expect(host.drafts.get("conv-tray")).toMatchObject({
        id: "draft-tray",
        recipientIds: ["+15550100"],
        revision: "1",
      }),
    );
  });

  it("opens native composer windows only through explicit actions", async () => {
    render(<App />);
    await screen.findByText("Hello from Aurora");
    expect(bridge.open_composer).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", { name: "Open in composer window" }),
    );
    await waitFor(() =>
      expect(bridge.open_composer).toHaveBeenCalledWith("aurora"),
    );
    fireEvent.click(screen.getByRole("button", { name: "New message window" }));
    await waitFor(() =>
      expect(bridge.open_composer).toHaveBeenLastCalledWith(undefined),
    );
  });
});

describe("gateway routes", () => {
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
      within(screen.getByRole("region", { name: "Gateway and SIM" })).getByText(
        /does not support MMS attachments/,
      ),
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
      within(screen.getByRole("region", { name: "Gateway and SIM" })).getByText(
        /no longer reported/,
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.keyDown(message(), { key: "Enter" });
    await Promise.resolve();
    expect(bridge.send_draft).not.toHaveBeenCalled();
    expect(bridge.save_draft).not.toHaveBeenCalled();
  });
});

describe("host state display", () => {
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

  it("marks only intersecting rows while the document is focused and visible", async () => {
    const mark = vi.mocked(bridge.mark_seen);
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
    await waitFor(() => expect(mark).toHaveBeenCalledWith(["m-aurora"]));
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
      recipientIds: ["+15550100"],
      attachmentIds: [],
      expectedRevision: "0",
    });
    expect(created.id).not.toBe("");
    expect(created.conversationId).not.toBe("");
    expect(created).toMatchObject({
      recipientIds: ["+15550100"],
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
      recipientIds: ["+15550100"],
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
    fireEvent.change(picker, { target: { value: "+1 555 0199" } });
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
      screen.getByRole("button", { name: /\+1 555 0199/ }),
    ).toHaveAttribute("data-conversation-id", assigned.conversationId);
  });
});
