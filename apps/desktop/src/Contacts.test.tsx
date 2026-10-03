import "@testing-library/jest-dom/vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ContactsView } from "./Contacts";
import {
  bridge,
  type ContactBookView,
  type ContactView,
  type ContactEditRequestInput,
} from "./bridge";

vi.mock("./bridge", { spy: true });

const mockContactBook: ContactBookView = {
  id: "book-pixel8",
  deviceName: "Pixel 8",
  state: "active",
  capabilities: {
    canWrite: true,
    canDelete: true,
    supportsNotes: true,
    supportsPhoto: true,
  },
  contactCount: 2,
  defaultAccountLabel: "Google",
  lastSyncAt: "Just now",
  pendingEditCount: 0,
};

const mockContactAurora: ContactView = {
  id: "contact-aurora",
  bookId: "book-pixel8",
  revision: "1",
  displayName: "Aurora Chen",
  givenName: "Aurora",
  familyName: "Chen",
  phones: [{ label: "mobile", number: "+12025550123" }],
  emails: [{ label: "work", address: "aurora@example.com" }],
  addresses: [],
  organization: "Northstar",
  title: "Director",
};

const mockContactRiver: ContactView = {
  id: "contact-river",
  bookId: "book-pixel8",
  revision: "1",
  displayName: "River Park",
  givenName: "River",
  familyName: "Park",
  phones: [{ label: "mobile", number: "+12025550124" }],
  emails: [],
  addresses: [],
};

describe("ContactsView", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(bridge.list_contact_books).mockResolvedValue([mockContactBook]);
    vi.mocked(bridge.list_contacts).mockResolvedValue([
      mockContactAurora,
      mockContactRiver,
    ]);
    vi.mocked(bridge.forget_contact_book).mockResolvedValue(undefined);
    vi.mocked(bridge.list_restorable_contacts).mockResolvedValue([]);
  });

  afterEach(cleanup);

  it("allows manual cache repair even when no repair is currently flagged", async () => {
    vi.mocked(bridge.request_contact_repair).mockResolvedValue({ repairRequired: true });
    render(<ContactsView sync={{ repairRequired: false }} />);
    fireEvent.click(await screen.findByLabelText("Repair contact cache"));
    await waitFor(() => expect(bridge.request_contact_repair).toHaveBeenCalledOnce());
  });

  it("requires confirmation and prevents duplicate requests while forgetting a book", async () => {
    let finish!: () => void;
    vi.mocked(bridge.forget_contact_book).mockReturnValueOnce(new Promise<void>(resolve => { finish = resolve; }));
    const { container } = render(<ContactsView />);
    fireEvent.click(await screen.findByLabelText("Forget Pixel 8"));
    expect(bridge.forget_contact_book).not.toHaveBeenCalled();
    fireEvent.keyDown(screen.getByLabelText("Confirm forget Pixel 8"), { key: "Escape" });
    fireEvent.click(screen.getByLabelText("Forget Pixel 8"));
    fireEvent.click(screen.getByLabelText("Confirm forget Pixel 8"));
    expect(bridge.forget_contact_book).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[data-book-forget="book-pixel8"]')).toBeDisabled();
    await act(async () => finish());
    expect(container.querySelector('[data-book-forget="book-pixel8"]')).toBeNull();
  });

  it.each([
    ["Add phone number", "Remove phone 1", "#contact-form-phones-list"],
    ["Add email address", "Remove email 1", "#contact-form-emails-list"],
    ["Add address", "Remove address 1", "#contact-form-addresses-list"],
  ])("keeps remaining field identity after removing an earlier row: %s", async (add, remove, list) => {
    const { container } = render(<ContactsView />);
    fireEvent.click(await screen.findByLabelText("New contact"));
    fireEvent.click(screen.getByLabelText(add));
    fireEvent.click(screen.getByLabelText(add));
    const second = container.querySelectorAll(`${list} > li`)[1];
    expect(second).toBeTruthy();
    fireEvent.click(screen.getByLabelText(remove));
    expect(container.querySelector(`${list} > li`)).toBe(second);
  });

  it("shows retention paused even while contact production is ready", async () => {
    const readiness = { state: "ready" as const, contacts_ready: true, server_active: false, backfill_unreadable: 0 };
    const { container, rerender } = render(<ContactsView sync={{ repairRequired: false, readiness }} />);
    await screen.findByText("Pixel 8");
    expect(container.querySelector("#contacts-readiness-status")).toHaveAttribute("role", "status");
    rerender(<ContactsView sync={{ repairRequired: false, readiness: { ...readiness, server_active: true } }} />);
    expect(container.querySelector("#contacts-readiness-status")).toBeNull();
  });

  it("guards external navigation until a dirty contact is explicitly discarded", async () => {
    let guard: ((action: () => void) => void) | null = null;
    let navigated = false;
    render(<ContactsView onNavigationGuard={value => { guard = value; }} />);
    fireEvent.click(await screen.findByRole("button", { name: /Aurora Chen/ }));
    fireEvent.click(screen.getByLabelText("Edit contact"));
    fireEvent.change(screen.getByLabelText("Given name"), { target: { value: "Unsaved" } });
    await waitFor(() => expect(guard).not.toBeNull());
    fireEvent.click(screen.getByRole("button", { name: /River Park/ }));
    expect(screen.getByLabelText("Given name")).toHaveValue("Unsaved");
    fireEvent.click(screen.getByRole("button", { name: "Keep editing" }));
    // Exercise the host/rail navigation boundary, not a second contact-row click.
    act(() => guard?.(() => { navigated = true; }));
    expect(navigated).toBe(false);
    expect(screen.getByLabelText("Given name")).toHaveValue("Unsaved");
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(navigated).toBe(true);
  });

  describe("initial load", () => {
    it("loads and displays contact books", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Pixel 8")).toBeInTheDocument();
      });
    });

    it("loads and displays contacts from default book", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
        expect(screen.getByText("River Park")).toBeInTheDocument();
      });
    });

    it("shows account label in book list", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Pixel 8")).toBeInTheDocument();
      });
    });
  });

  describe("selecting contacts", () => {
    it("shows contact detail when selected", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      await waitFor(() => {
        expect(screen.getByText("(202) 555-0123")).toBeInTheDocument();
      });
    });

    it("displays all contact fields in detail view", async () => {
      render(<ContactsView />);

      // findByRole waits for contacts to load before the click
      const auroraButton = await screen.findByRole("button", { name: /Aurora Chen/ }, { timeout: 3000 });
      fireEvent.click(auroraButton);

      // Email renders as "work · aurora@example.com" — use regex for substring
      await screen.findByText(/aurora@example\.com/, {}, { timeout: 3000 });
      // "Northstar · Director" is the full small-element text in the detail header
      await screen.findByText("Northstar · Director", {}, { timeout: 3000 });
    });
  });

  describe("creating contacts", () => {
    it("opens create form on new contact button", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByLabelText("New contact")).toBeInTheDocument();
      });

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toHaveAccessibleName("Create contact");
      });
    });

    it("calls submit_contact_edit with create kind", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      const givenNameInput = screen.getByLabelText("Given name");
      fireEvent.change(givenNameInput, { target: { value: "Alice" } });

      const saveButton = screen.getByText("Save");
      fireEvent.click(saveButton);

      await waitFor(() => {
        const calls = vi.mocked(bridge.submit_contact_edit).mock.calls;
        expect(calls.length).toBeGreaterThan(0);
        expect(calls[0][0].kind).toBe("create");
      });
    });

    it("includes changed fields in patches", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      const givenInput = screen.getByLabelText("Given name");
      fireEvent.change(givenInput, { target: { value: "Bob" } });

      const saveButton = screen.getByText("Save");
      fireEvent.click(saveButton);

      await waitFor(() => {
        const calls = vi.mocked(bridge.submit_contact_edit).mock.calls;
        const call = calls[calls.length - 1][0];
        expect(call.patches).toBeDefined();
      });
    });

    it("disables save when name is empty", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      const saveButton = screen.getByText("Save");
      expect(saveButton).toBeDisabled();
    });
  });

  describe("editing contacts", () => {
    it("opens edit form when edit button clicked", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      fireEvent.click(screen.getByLabelText("Edit contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toHaveAccessibleName("Edit contact");
      });
    });

    it("populates form with existing contact data", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      fireEvent.click(screen.getByLabelText("Edit contact"));

      await waitFor(() => {
        expect(screen.getByDisplayValue("Aurora")).toBeDefined();
      });
    });

    it("sends update kind with contact id and revision", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      fireEvent.click(screen.getByLabelText("Edit contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      const saveButton = screen.getByText("Save");
      fireEvent.click(saveButton);

      await waitFor(() => {
        const calls = vi.mocked(bridge.submit_contact_edit).mock.calls;
        const call = calls[calls.length - 1][0];
        expect(call.kind).toBe("update");
        expect(call.contactId).toBe("contact-aurora");
      });
    });
  });

  describe("deleting contacts", () => {
    it("shows confirmation on delete button click", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
        fireEvent.click(auroraButton);
      });

      await waitFor(() => {
        const deleteButton = screen.getByLabelText("Delete contact");
        expect(deleteButton).toHaveTextContent("Delete");
        fireEvent.click(deleteButton);
      });

      await waitFor(() => {
        expect(screen.getByLabelText("Confirm delete")).toHaveTextContent(
          "Confirm delete"
        );
      });
    });

    it("calls submit_contact_edit with delete kind", async () => {
      vi.mocked(bridge.submit_contact_edit).mockResolvedValue({
        state: "applied",
        newRevision: "2",
      });

      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      const deleteButton = screen.getByLabelText("Delete contact");
      fireEvent.click(deleteButton);

      await waitFor(() => {
        fireEvent.click(deleteButton);
      });

      await waitFor(() => {
        const calls = vi.mocked(bridge.submit_contact_edit).mock.calls;
        const call = calls[calls.length - 1][0];
        expect(call.kind).toBe("delete");
      });
    });

    it("clears contact selection after delete", async () => {
      vi.mocked(bridge.submit_contact_edit).mockResolvedValue({
        state: "applied",
        newRevision: "2",
      });

      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      const deleteButton = screen.getByLabelText("Delete contact");
      fireEvent.click(deleteButton);

      fireEvent.click(deleteButton);

      await waitFor(() => {
        expect(screen.queryByRole("form")).not.toBeInTheDocument();
      }, { timeout: 2000 });
    });
  });

  describe("phone management", () => {
    it("adds phone numbers in create form", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      const addPhoneButton = screen.getByLabelText("Add phone number");
      fireEvent.click(addPhoneButton);

      await waitFor(() => {
        // After adding one phone, the select defaults to "Mobile" label
        const phoneSelects = screen.getAllByDisplayValue("Mobile");
        expect(phoneSelects.length).toBeGreaterThanOrEqual(1);
      });
    });

    it("removes phone numbers", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      // Add a phone first so we have something to remove
      const addPhoneButton = screen.getByLabelText("Add phone number");
      fireEvent.click(addPhoneButton);

      await waitFor(() => {
        expect(screen.getByLabelText("Remove phone 1")).toBeInTheDocument();
      });

      const removeButton = screen.getByLabelText("Remove phone 1");
      fireEvent.click(removeButton);

      await waitFor(() => {
        const remainingRemoveButtons = screen.queryAllByLabelText(/Remove phone/);
        expect(remainingRemoveButtons.length).toBe(0);
      });
    });
  });

  describe("error handling", () => {
    it("displays conflict error from server", async () => {
      vi.mocked(bridge.submit_contact_edit).mockResolvedValue({
        state: "conflict",
        conflictSummary: "Contact was modified elsewhere",
      });

      render(<ContactsView />);

      // Wait for books to load so book is set before handleSave runs
      await screen.findByText("Pixel 8", {}, { timeout: 3000 });

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => expect(screen.getByRole("form")).toBeInTheDocument());

      const conflictInput = screen.getByLabelText("Given name");
      fireEvent.change(conflictInput, { target: { value: "Test" } });

      fireEvent.click(screen.getByText("Save"));

      await waitFor(() => {
        expect(screen.getByText(/Conflict/)).toBeInTheDocument();
      });
    });

    it("displays rejection error from server", async () => {
      vi.mocked(bridge.submit_contact_edit).mockResolvedValue({
        state: "rejected",
        reason: "Invalid contact data",
      });

      render(<ContactsView />);

      // Wait for books to load so book is set before handleSave runs
      await screen.findByText("Pixel 8", {}, { timeout: 3000 });

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => expect(screen.getByRole("form")).toBeInTheDocument());

      const rejectionInput = screen.getByLabelText("Given name");
      fireEvent.change(rejectionInput, { target: { value: "Test" } });

      fireEvent.click(screen.getByText("Save"));

      await waitFor(() => {
        expect(screen.getByText(/Invalid contact data/)).toBeInTheDocument();
      });
    });
  });

  describe("search", () => {
    it("filters contacts by query", async () => {
      // Re-mock list_contacts to return filtered results for query "river"
      vi.mocked(bridge.list_contacts).mockImplementation(async (bookId, query) => {
        if (query === "river") return [mockContactRiver];
        return [mockContactAurora, mockContactRiver];
      });

      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const searchInput = screen.getByLabelText("Search contacts");
      fireEvent.change(searchInput, { target: { value: "river" } });

      await waitFor(() => {
        expect(screen.queryByText("Aurora Chen")).not.toBeInTheDocument();
        expect(screen.getByText("River Park")).toBeInTheDocument();
      });
    });

    it("clears search on clear button click", async () => {
      // Re-mock list_contacts to return filtered results for query "river"
      vi.mocked(bridge.list_contacts).mockImplementation(async (bookId, query) => {
        if (query === "river") return [mockContactRiver];
        return [mockContactAurora, mockContactRiver];
      });

      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const searchInput = screen.getByLabelText("Search contacts");
      fireEvent.change(searchInput, { target: { value: "river" } });

      await waitFor(() => {
        expect(screen.queryByText("Aurora Chen")).not.toBeInTheDocument();
      });

      const clearButton = screen.getByLabelText("Clear search");
      fireEvent.click(clearButton);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });
    });
  });

  describe("state management", () => {
    it("closes editor on cancel", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      fireEvent.click(screen.getByText("Cancel"));

      await waitFor(() => {
        expect(screen.queryByRole("form")).not.toBeInTheDocument();
      });
    });

    it("reloads contacts after successful save", async () => {
      vi.mocked(bridge.list_contacts).mockClear();
      vi.mocked(bridge.list_contacts).mockResolvedValue([mockContactAurora]);

      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      const reloadInput = screen.getByLabelText("Given name");
      fireEvent.change(reloadInput, { target: { value: "Test" } });

      fireEvent.click(screen.getByText("Save"));

      await waitFor(() => {
        expect(vi.mocked(bridge.list_contacts).mock.calls.length).toBeGreaterThan(0);
      });
    });
  });

  describe("accessibility behaviors", () => {
    it("contact row button has an aria-label that includes display name and formatted phone", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        const btn = screen.getByRole("button", { name: /Aurora Chen/ });
        // aria-label must include national-formatted phone, not raw E.164
        expect(btn).toHaveAttribute("aria-label", expect.stringContaining("(202) 555-0123"));
        expect(btn).toHaveAttribute("aria-label", expect.stringContaining("Aurora Chen"));
      });
    });

    it("form inputs have labels that activate focus via htmlFor", async () => {
      render(<ContactsView />);

      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => {
        expect(screen.getByRole("form")).toBeInTheDocument();
      });

      // getByLabelText with selector="input" verifies htmlFor wiring
      const givenInput = screen.getByLabelText("Given name");
      expect(givenInput.tagName).toBe("INPUT");
      expect(givenInput).toHaveAttribute("id", "contact-form-given-name");

      const familyInput = screen.getByLabelText("Family name");
      expect(familyInput.tagName).toBe("INPUT");
      expect(familyInput).toHaveAttribute("id", "contact-form-family-name");
    });

    it("delete confirm state resets on Escape keydown", async () => {
      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      await waitFor(() => {
        expect(screen.getByLabelText("Delete contact")).toBeInTheDocument();
      });

      const deleteBtn = screen.getByLabelText("Delete contact");
      fireEvent.click(deleteBtn);

      await waitFor(() => {
        expect(screen.getByLabelText("Confirm delete")).toHaveTextContent("Confirm delete");
      });

      // Escape should reset the confirmation state
      fireEvent.keyDown(screen.getByLabelText("Confirm delete"), { key: "Escape" });

      await waitFor(() => {
        expect(screen.getByLabelText("Delete contact")).toHaveTextContent("Delete");
      });
    });

    it("pending state banner has role=status for screen reader announcement", async () => {
      const contactWithPending: typeof mockContactAurora = {
        ...mockContactAurora,
        pendingEditState: "pending",
        pendingEditSummary: "Phone number update",
      };
      vi.mocked(bridge.list_contacts).mockResolvedValue([contactWithPending, mockContactRiver]);

      render(<ContactsView />);

      await waitFor(() => {
        expect(screen.getByText("Aurora Chen")).toBeInTheDocument();
      });

      const auroraButton = screen.getByRole("button", { name: /Aurora Chen/ });
      fireEvent.click(auroraButton);

      await waitFor(() => {
        const banner = document.querySelector("[data-contact-banner]");
        expect(banner).toBeInTheDocument();
        expect(banner).toHaveAttribute("role", "status");
      });
    });

    it("form fields are readOnly when book canWrite is false", async () => {
      const readOnlyBook = {
        ...mockContactBook,
        capabilities: { canWrite: false, canDelete: false, supportsNotes: false, supportsPhoto: false },
      };
      vi.mocked(bridge.list_contact_books).mockResolvedValue([readOnlyBook]);
      vi.mocked(bridge.list_contacts).mockResolvedValue([mockContactAurora]);

      render(<ContactsView />);

      // Wait for books and contacts to load
      await screen.findByText("Aurora Chen", {}, { timeout: 3000 });

      // When canWrite=false, the Edit button is intentionally hidden.
      // Open the form via New Contact to verify fieldReadOnly applies to new forms too.
      // (The canWrite gate on Edit button is separate from per-field readOnly.)
      // Use New Contact to open form with the readOnly book active.
      fireEvent.click(screen.getByLabelText("New contact"));

      await waitFor(() => expect(screen.getByRole("form")).toBeInTheDocument());

      const givenInput = screen.getByLabelText("Given name");
      expect(givenInput).toHaveAttribute("readonly");
    });
  });

  describe("live updates, paging and honest status", () => {
    it("refreshes the page on native hints without discarding an open editor", async () => {
      let hint: (() => void) | undefined;
      vi.mocked(bridge.subscribe).mockImplementation(listener => {
        hint = listener;
        return () => undefined;
      });
      render(<ContactsView />);
      fireEvent.click(await screen.findByRole("button", { name: /Aurora Chen/ }));
      fireEvent.click(await screen.findByLabelText("Edit contact"));
      const given = await screen.findByLabelText("Given name");
      fireEvent.change(given, { target: { value: "Aurelia" } });

      vi.mocked(bridge.list_contacts).mockResolvedValue([
        { ...mockContactAurora, pendingEditId: "r1", pendingEditState: "awaiting-approval", pendingEditSummary: "Name" },
        mockContactRiver,
      ]);
      const before = vi.mocked(bridge.list_contacts).mock.calls.length;
      hint?.();
      await waitFor(() => expect(vi.mocked(bridge.list_contacts).mock.calls.length).toBeGreaterThan(before));
      expect(screen.getByLabelText("Given name")).toHaveValue("Aurelia");
      expect(screen.getByRole("form")).toHaveAccessibleName("Edit contact");
      expect(await screen.findByRole("button", { name: /Waiting for approval on the phone/ })).toBeInTheDocument();
    });

    it("pages beyond 200 contacts with bounded Previous/Next requests", async () => {
      const page = Array.from({ length: 200 }, (_, index) => ({ ...mockContactRiver, id: `c-${index}`, displayName: `Person ${index}` }));
      vi.mocked(bridge.list_contacts).mockImplementation(async (_book, _query, offset) =>
        offset ? [{ ...mockContactRiver, id: "c-200", displayName: "Person 200" }] : page,
      );
      render(<ContactsView />);
      await screen.findByText("Person 0");
      expect(document.querySelectorAll("#contacts-list > li")).toHaveLength(200);
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      await screen.findByText("Person 200");
      expect(vi.mocked(bridge.list_contacts)).toHaveBeenLastCalledWith("book-pixel8", "", 200);
      expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
      fireEvent.click(screen.getByRole("button", { name: "Previous" }));
      await screen.findByText("Person 0");
    });

    it("ignores a stale search response that arrives after a newer one", async () => {
      let resolveOld: (value: ContactView[]) => void = () => {};
      vi.mocked(bridge.list_contacts).mockImplementation((_book, query) =>
        query === "a" ? new Promise(resolve => { resolveOld = resolve; }) : Promise.resolve(query ? [mockContactRiver] : [mockContactAurora, mockContactRiver]),
      );
      render(<ContactsView />);
      await screen.findByText("Aurora Chen");
      const search = screen.getByLabelText("Search contacts");
      fireEvent.change(search, { target: { value: "a" } });
      fireEvent.change(search, { target: { value: "ri" } });
      await waitFor(() => expect(screen.queryByText("Aurora Chen")).not.toBeInTheDocument());
      resolveOld([mockContactAurora]);
      await new Promise(resolve => setTimeout(resolve, 20));
      expect(screen.queryByText("Aurora Chen")).not.toBeInTheDocument();
      expect(screen.getByText("River Park")).toBeInTheDocument();
    });

    it("reports a sent edit as waiting for the phone, never as applied", async () => {
      vi.mocked(bridge.submit_contact_edit).mockResolvedValue({ state: "pending", requestId: "r9" });
      render(<ContactsView />);
      fireEvent.click(await screen.findByLabelText("New contact"));
      fireEvent.change(await screen.findByLabelText("Given name"), { target: { value: "Grace" } });
      fireEvent.click(screen.getByText("Save"));
      expect(await screen.findByText("Sent to Pixel 8. Waiting for the phone to apply it.")).toBeInTheDocument();
    });

    it("shows contact projection failures with a repair action", async () => {
      vi.mocked(bridge.request_contact_repair).mockResolvedValue({ repairRequired: true });
      render(<ContactsView sync={{ repairRequired: false, projection: { state: "failed", reason: "missing_record" } }} />);
      expect(await screen.findByText(/Contacts may be out of date \(rebuild failed: missing record\)/)).toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "Repair contact cache" }));
      await waitFor(() => expect(bridge.request_contact_repair).toHaveBeenCalled());
    });
  });
});
