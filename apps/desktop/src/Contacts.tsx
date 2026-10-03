import { useCallback, useEffect, useRef, useState } from "react";
import { ContactAvatar, Clock, MessageSquarePlus, X, formatPhoneNumber } from "@peppy/desktop-ui";
import { bridge, type ContactBookView, type ContactSyncStatus, type ContactView, type RestorableContact } from "./bridge";
import { ContactForm, type ContactEditDraft } from "./ContactForm";
import "./contacts.css";

/** Core returns at most this many contacts per page (search runs before paging). */
export const CONTACTS_PAGE_SIZE = 200;

const EDIT_STATE_LABELS: Record<string, string> = {
  pending: "Waiting for the phone",
  "awaiting-approval": "Waiting for approval on the phone",
  "outcome-unknown": "Outcome unknown — check the phone",
  conflict: "Conflict — changed on the phone",
  rejected: "Rejected by the phone",
  failed: "Failed on the phone",
  expired: "Expired before the phone applied it",
};
export const editStateLabel = (state: string) => EDIT_STATE_LABELS[state] ?? state;

export type ContactNavigationGuard = (navigate: () => void) => void;

export function ContactsView({ books: initialBooks = [], sync, onNavigationGuard }: {
  books?: ContactBookView[];
  sync?: ContactSyncStatus;
  onNavigationGuard?(guard: ContactNavigationGuard | null): void;
}) {
  const [books, setBooks] = useState(initialBooks);
  const [bookId, setBookId] = useState(initialBooks[0]?.id ?? "");
  const [contacts, setContacts] = useState<ContactView[]>([]);
  const [selected, setSelected] = useState<ContactView | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [restore, setRestore] = useState<RestorableContact[] | null>(null);
  const [editing, setEditing] = useState<ContactView | null | "create">(null);
  const [error, setError] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmForget, setConfirmForget] = useState<string | null>(null);
  const [forgetting, setForgetting] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [offset, setOffset] = useState(0);
  const [repairing, setRepairing] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [pendingNavigation, setPendingNavigation] = useState<(() => void) | null>(null);
  const restoreTriggerRef = useRef<HTMLButtonElement>(null);
  // Only the newest list request may apply its result (book/search/page changes and live hints).
  const pageRequestRef = useRef(0);

  const loadBooks = useCallback(() => {
    void bridge
      .list_contact_books()
      .then(next => {
        setBooks(next);
        setBookId(current => (current && next.some(book => book.id === current) ? current : next[0]?.id ?? ""));
      })
      .catch(e => setError(String(e?.message ?? e)));
  }, []);

  const loadPage = useCallback((showLoading: boolean) => {
    if (!bookId) return;
    const request = ++pageRequestRef.current;
    if (showLoading) setLoading(true);
    void bridge
      .list_contacts(bookId, query, offset)
      .then(results => {
        if (request !== pageRequestRef.current) return;
        setContacts(results);
        // Keep the open detail current; an open editor keeps its unsaved draft untouched.
        setSelected(current => (current ? results.find(item => item.id === current.id) ?? current : current));
      })
      .catch(e => {
        if (request === pageRequestRef.current) setError(String(e?.message ?? e));
      })
      .finally(() => {
        if (request === pageRequestRef.current) setLoading(false);
      });
  }, [bookId, query, offset]);

  useEffect(loadBooks, [loadBooks]);
  useEffect(() => loadPage(true), [loadPage]);
  // Native state hints (sync, owner results) refresh books, the visible page and its badges.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const dispose = bridge.subscribe(() => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        loadBooks();
        loadPage(false);
      }, 150);
    });
    return () => {
      clearTimeout(timer);
      dispose();
    };
  }, [loadBooks, loadPage]);
  useEffect(() => setOffset(0), [bookId, query]);

  const book = books.find(item => item.id === bookId);

  const handleSave = async (draft: ContactEditDraft) => {
    if (!book || !editing) return;
    setError("");
    setBusy(true);

    try {
      const isCreate = editing === "create";
      const current = isCreate ? undefined : editing;

      // Build patches from draft changes
      const patches: unknown[] = [];

      if (current) {
        // Update case: build patches for changed fields
        if ((current.givenName ?? "") !== (draft.givenName ?? "")) {
          patches.push({ field: "givenName", value: draft.givenName, expectedOld: current.givenName });
        }
        if ((current.familyName ?? "") !== (draft.familyName ?? "")) {
          patches.push({ field: "familyName", value: draft.familyName, expectedOld: current.familyName });
        }
        if ((current.nickname ?? "") !== (draft.nickname ?? "")) {
          patches.push({ field: "nickname", value: draft.nickname, expectedOld: current.nickname });
        }
        if ((current.organization ?? "") !== (draft.organization ?? "")) {
          patches.push({ field: "organization", value: draft.organization, expectedOld: current.organization });
        }
        if ((current.title ?? "") !== (draft.title ?? "")) {
          patches.push({ field: "title", value: draft.title, expectedOld: current.title });
        }
        if ((current.notes ?? "") !== (draft.notes ?? "")) {
          patches.push({ field: "notes", value: draft.notes, expectedOld: current.notes });
        }

        // Phone comparison
        if (JSON.stringify(current.phones) !== JSON.stringify(draft.phones)) {
          patches.push({ field: "phones", value: draft.phones, expectedOld: current.phones });
        }

        // Email comparison
        if (JSON.stringify(current.emails) !== JSON.stringify(draft.emails)) {
          patches.push({ field: "emails", value: draft.emails, expectedOld: current.emails });
        }

        // Address comparison
        if (JSON.stringify(current.addresses) !== JSON.stringify(draft.addresses)) {
          patches.push({ field: "addresses", value: draft.addresses, expectedOld: current.addresses });
        }

        // Birthday comparison
        if (JSON.stringify(current.birthday ?? {}) !== JSON.stringify(draft.birthday ?? {})) {
          patches.push({ field: "birthday", value: draft.birthday, expectedOld: current.birthday });
        }
      } else {
        // Create case: include all fields
        patches.push({ field: "givenName", value: draft.givenName });
        patches.push({ field: "familyName", value: draft.familyName });
        patches.push({ field: "nickname", value: draft.nickname });
        patches.push({ field: "organization", value: draft.organization });
        patches.push({ field: "title", value: draft.title });
        patches.push({ field: "phones", value: draft.phones });
        patches.push({ field: "emails", value: draft.emails });
        patches.push({ field: "addresses", value: draft.addresses });
        patches.push({ field: "birthday", value: draft.birthday });
        patches.push({ field: "notes", value: draft.notes });
      }

      const photoKind = draft.photoKind;
      const photoRequest =
        photoKind === "keep"
          ? { kind: "keep" as const }
          : photoKind === "remove"
            ? { kind: "remove" as const }
            : { kind: "set" as const, croppedDataUrl: draft.photoDataUrl! };

      const outcome = await bridge.submit_contact_edit({
        targetBookId: book.id,
        kind: isCreate ? "create" : "update",
        contactId: current?.id,
        baseRevision: current?.revision,
        patches,
        photo: photoRequest,
      });

      if (outcome.state === "rejected") {
        setError(outcome.reason);
        setBusy(false);
        return;
      }

      if (outcome.state === "conflict") {
        setError(`Conflict: ${outcome.conflictSummary}`);
        setBusy(false);
        return;
      }

      // Sent, not applied: the phone owns the change until its result arrives.
      setEditing(null);
      setDirty(false);
      setSelected(null);
      setError("");
      setNotice(outcome.state === "pending" ? `Sent to ${book.deviceName}. Waiting for the phone to apply it.` : "");
      loadPage(false);
      if (pendingNavigation) {
        setPendingNavigation(null);
        pendingNavigation();
      }
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async () => {
    if (!selected || !book) return;

    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }

    setError("");
    setBusy(true);

    try {
      const outcome = await bridge.submit_contact_edit({
        targetBookId: book.id,
        kind: "delete",
        contactId: selected.id,
        baseRevision: selected.revision,
        patches: [],
        photo: { kind: "keep" },
      });

      if (outcome.state === "rejected") {
        setError(outcome.reason);
        setBusy(false);
        return;
      }

      if (outcome.state === "conflict") {
        setError(`Conflict: ${outcome.conflictSummary}`);
        setBusy(false);
        return;
      }

      setSelected(null);
      setConfirmDelete(false);
      setError("");
      setNotice(outcome.state === "pending" ? `Delete sent to ${book.deviceName}. Waiting for the phone.` : "");
      loadPage(false);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  };

  const requestNavigation = useCallback((action: () => void) => {
    if (busy) return;
    setConfirmForget(null);
    if (editing && dirty) {
      setPendingNavigation(() => action);
      return;
    }
    action();
  }, [busy, dirty, editing]);

  useEffect(() => {
    onNavigationGuard?.(requestNavigation);
    return () => onNavigationGuard?.(null);
  }, [onNavigationGuard, requestNavigation]);

  const discardNavigation = () => {
    if (!pendingNavigation || busy) return;
    setDirty(false);
    const action = pendingNavigation;
    setPendingNavigation(null);
    action();
  };

  const handleSelectBook = (id: string) => requestNavigation(() => {
    setBookId(id);
    setSelected(null);
    setEditing(null);
    setConfirmDelete(false);
    setError("");
  });

  const handleCancelConfirm = () => setConfirmDelete(false);

  const handleSelectContact = (contact: ContactView) => requestNavigation(() => {
    setSelected(contact);
    setEditing(null);
    setConfirmDelete(false);
  });

  const handleEditStart = (contact?: ContactView) => requestNavigation(() => {
    setEditing(contact ?? "create");
    setDirty(false);
    setConfirmDelete(false);
    setError("");
  });

  const handleEditCancel = () => requestNavigation(() => {
    setEditing(null);
    setDirty(false);
    setError("");
  });

  const handleForget = (id: string) => {
    if (forgetting) return;
    if (confirmForget !== id) { setConfirmForget(id); return; }
    setConfirmForget(null);
    requestNavigation(() => {
      setForgetting(id);
      setError("");
      void bridge.forget_contact_book(id)
        .then(() => setBooks(all => all.filter(value => value.id !== id)))
        .catch(e => setError(String(e?.message ?? e)))
        .finally(() => setForgetting(null));
    });
  };

  return (
    <section id="contacts-view" role="region" aria-label="Contacts browser">
      <aside id="contacts-book-list" aria-label="Contact books">
        <header id="contacts-book-list-toolbar">
          <h2>Contacts</h2>
          <button
            id="contacts-repair"
            className="secondary-button"
            data-action="repair-contacts"
            aria-label="Repair contact cache"
            disabled={repairing || sync?.projection?.state === "rebuilding"}
            onClick={() => {
              setRepairing(true);
              void bridge.request_contact_repair()
                .then(() => setNotice("Contact cache repair requested."))
                .catch(e => setError(String(e?.message ?? e)))
                .finally(() => setRepairing(false));
            }}
          >{repairing ? "Requesting…" : "Repair"}</button>
        </header>
        <ul id="contacts-books">
          {books.map(item => (
            <li key={item.id}>
              <button
                data-book-id={item.id}
                className="contacts-book-row"
                aria-current={item.id === bookId ? "location" : undefined}
                onClick={() => handleSelectBook(item.id)}
              >
                <span>📱</span>
                <span>
                  <b>{item.deviceName}</b>
                  <small data-book-state={item.state}>
                    {item.state === "active" ? "Active" : item.state === "limited" ? "Limited access" : item.state === "retired" ? "Retired" : "Unavailable"}
                    {item.state === "active" ? ` · ${item.contactCount}` : ""}
                    {item.defaultAccountLabel && ` · ${item.defaultAccountLabel}`}
                  </small>
                </span>
              </button>
              <button
                data-book-forget={item.id}
                data-confirm-forget={confirmForget === item.id ? "pending" : undefined}
                className="secondary-button"
                aria-label={`${confirmForget === item.id ? "Confirm forget" : "Forget"} ${item.deviceName}`}
                title="Permanently hide this book on this desktop. Phone contacts are not changed."
                disabled={forgetting !== null || busy}
                onClick={() => handleForget(item.id)}
                onBlur={() => setConfirmForget(current => current === item.id ? null : current)}
                onKeyDown={event => { if (event.key === "Escape") setConfirmForget(null); }}
              >
                {forgetting === item.id ? "Forgetting…" : confirmForget === item.id ? "Confirm forget" : "Forget"}
              </button>
            </li>
          ))}
        </ul>
      </aside>

      <section id="contacts-center-pane">
        {pendingNavigation && (
          <p id="contacts-unsaved-changes" role="status" data-banner-state="unsaved-changes">
            You have unsaved changes.
            <button type="button" className="secondary-button" onClick={() => setPendingNavigation(null)}>Keep editing</button>
            <button type="button" className="secondary-button" onClick={discardNavigation} disabled={busy}>Discard</button>
          </p>
        )}
        {sync?.readiness && (sync.readiness.state !== "ready" || !sync.readiness.server_active) && (
          <p id="contacts-readiness-status" role="status" data-banner-state={sync.readiness.state}>
            {sync.readiness.state === "server_unsupported" ? "Contact changes need a server that supports contact sync." : sync.readiness.state === "needs_unlock" ? "Contact changes are paused until all required keys are unlocked." : sync.readiness.state === "backfill_pending" ? "Contact changes are preparing historical records." : "Contact sync is ready."}
            {sync.readiness.backfill_unreadable > 0 && " Some historical records remain unreadable."}
            {!sync.readiness.server_active && " History retention is paused until every device is ready."}
          </p>
        )}
        {(sync?.repairRequired || sync?.projection?.state === "failed" || sync?.projection?.state === "rebuilding") && (
          <p id="contacts-sync-status" role="status" data-sync-state={sync.projection?.state ?? "repair"}>
            {sync.projection?.state === "rebuilding"
              ? "Rebuilding contacts from the server…"
              : sync.projection?.state === "failed"
                ? `Contacts may be out of date (rebuild failed${sync.projection.reason ? `: ${sync.projection.reason.replace(/_/g, " ")}` : ""}).`
                : "Contacts need a repair from the server."}
          </p>
        )}
        {notice && (
          <p id="contacts-notice" role="status" data-banner-state="notice">
            {notice}
            <button aria-label="Dismiss" onClick={() => setNotice("")}>
              <X size={12} />
            </button>
          </p>
        )}
        <header id="contacts-center-toolbar">
          <input
            id="contacts-search"
            aria-label="Search contacts"
            type="search"
            value={query}
            onChange={e => setQuery(e.target.value)}
          />
          {query && (
            <button aria-label="Clear search" onClick={() => setQuery("")}>
              <X size={14} />
            </button>
          )}
          <button
            data-action="create-contact"
            aria-label="New contact"
            onClick={() => handleEditStart()}
          >
            <MessageSquarePlus size={16} />
          </button>
          <button
            ref={restoreTriggerRef}
            aria-label="Recently deleted contacts"
            onClick={() => {
              setConfirmDelete(false);
              void bridge
                .list_restorable_contacts(bookId)
                .then(setRestore)
                .catch(e => setError(String(e.message ?? e)));
            }}
          >
            <Clock size={16} />
          </button>
        </header>

        {restore ? (
          <Restore
            list={restore}
            canRestore={Boolean(book?.capabilities.canRestore)}
            onClose={() => { setRestore(null); restoreTriggerRef.current?.focus(); }}
            onRestore={async value => {
              try {
                const outcome = await bridge.restore_contact(bookId, value.id);
                if (outcome.state === "rejected") {
                  setError(outcome.reason);
                } else if (outcome.state === "conflict") {
                  setError(`Conflict: ${outcome.conflictSummary}`);
                } else {
                  setRestore(all => all?.filter(item => item.id !== value.id) ?? null);
                  if (outcome.state === "pending") setNotice(`Restore of ${value.displayName} sent. Waiting for the phone.`);
                  loadPage(false);
                }
              } catch (e) {
                setError(String(e instanceof Error ? e.message : e));
              }
            }}
          />
        ) : (
          <ul id="contacts-list" aria-busy={loading}>
            {contacts.map(contact => (
              <li key={contact.id}>
                <button
                  data-contact-id={contact.id}
                  className="contacts-row"
                  aria-label={[contact.displayName, contact.phones[0]?.number && formatPhoneNumber(contact.phones[0].number), contact.pendingEditState && editStateLabel(contact.pendingEditState)].filter(Boolean).join(", ")}
                  aria-current={selected?.id === contact.id ? "location" : undefined}
                  onClick={() => handleSelectContact(contact)}
                >
                  <ContactAvatar
                    name={contact.displayName}
                    photoDataUrl={contact.photoDataUrl}
                  />
                  <span className="contacts-row-copy">
                    <b>{contact.displayName}</b>
                    <small>{contact.phones[0]?.number ? formatPhoneNumber(contact.phones[0].number) : ""}</small>
                  </span>
                  {contact.pendingEditState && (
                    <span
                      className="contacts-row-state-badge"
                      data-state={contact.pendingEditState}
                    >
                      <span className="sr-only">{editStateLabel(contact.pendingEditState)}</span>
                    </span>
                  )}
                </button>
              </li>
            ))}
            {!loading && !contacts.length && (
              <li id={query ? "contacts-search-empty" : "contacts-empty-state"}>
                {query
                  ? `No contacts match "${query}"`
                  : offset ? "No more contacts" : "No contacts in this book"}
              </li>
            )}
          </ul>
        )}
        {!restore && (offset > 0 || contacts.length >= CONTACTS_PAGE_SIZE) && (
          <nav id="contacts-pager" aria-label="Contact pages">
            <button
              data-action="contacts-previous-page"
              disabled={offset === 0 || loading}
              onClick={() => setOffset(current => Math.max(0, current - CONTACTS_PAGE_SIZE))}
            >
              Previous
            </button>
            <span>{`${offset + 1}–${offset + contacts.length}`}</span>
            <button
              data-action="contacts-next-page"
              disabled={contacts.length < CONTACTS_PAGE_SIZE || loading}
              onClick={() => setOffset(current => current + CONTACTS_PAGE_SIZE)}
            >
              Next
            </button>
          </nav>
        )}
      </section>

      {(selected || editing) && (
        <aside id="contacts-detail-panel" role="region" aria-label="Contact detail">
          {editing ? (
            <ContactForm
              initial={editing === "create" ? undefined : editing}
              onCancel={handleEditCancel}
              onSave={handleSave}
              onDirtyChange={setDirty}
              error={error}
              busy={busy}
              bookCapabilities={book?.capabilities}
            />
          ) : (
            selected && (
              <Detail
                contact={selected}
                canWrite={Boolean(book?.capabilities.canWrite)}
                confirm={confirmDelete}
                onEdit={() => handleEditStart(selected)}
                onDelete={handleDelete}
                onCancelConfirm={handleCancelConfirm}
              />
            )
          )}
        </aside>
      )}
    </section>
  );
}

function Detail({
  contact,
  canWrite,
  confirm,
  onEdit,
  onDelete,
  onCancelConfirm,
}: {
  contact: ContactView;
  canWrite: boolean;
  confirm: boolean;
  onEdit(): void;
  onDelete(): void;
  onCancelConfirm(): void;
}) {
  return (
    <>
      {contact.pendingEditState && (
        <section data-contact-banner={contact.pendingEditState} role="status">
          {contact.pendingEditSummary ?? "Change"} — {editStateLabel(contact.pendingEditState)}
        </section>
      )}
      <header id="contacts-detail-header">
        <ContactAvatar
          name={contact.displayName}
          photoDataUrl={contact.photoDataUrl}
          size={64}
        />
        <div>
          <h1>{contact.displayName}</h1>
          <small>{[contact.organization, contact.title].filter(Boolean).join(" · ")}</small>
        </div>
        {canWrite && (
          <div>
            <button
              data-action="edit-contact"
              aria-label="Edit contact"
              onClick={onEdit}
            >
              Edit
            </button>
            <button
              data-action="delete-contact"
              data-confirm-delete={confirm ? "pending" : undefined}
              aria-label={confirm ? "Confirm delete" : "Delete contact"}
              onKeyDown={e => { if (e.key === "Escape" && confirm) { e.preventDefault(); onCancelConfirm(); } }}
              onBlur={confirm ? onCancelConfirm : undefined}
              onClick={onDelete}
            >
              {confirm ? "Confirm delete" : "Delete"}
            </button>
          </div>
        )}
      </header>
      {contact.phones.length > 0 && (
        <section data-contact-section="phones">
          <h3>Phone numbers</h3>
          {contact.phones.map(phone => (
            <p key={`${phone.label}-${phone.number}`}>
              {phone.displayLabel ?? phone.label} · {formatPhoneNumber(phone.number)}
            </p>
          ))}
        </section>
      )}
      {contact.emails.length > 0 && (
        <section data-contact-section="emails">
          <h3>Email</h3>
          {contact.emails.map(email => (
            <p key={email.address}>
              {email.displayLabel ?? email.label} · {email.address}
            </p>
          ))}
        </section>
      )}
      {contact.addresses.length > 0 && (
        <section data-contact-section="addresses">
          <h3>Addresses</h3>
          {contact.addresses.map((addr, idx) => (
            <p key={`${addr.label ?? ""}-${addr.street ?? ""}-${addr.city ?? ""}`}>
              {addr.displayLabel ?? addr.label} · {[addr.street, addr.city, addr.state, addr.postalCode, addr.country]
                .filter(Boolean)
                .join(", ")}
            </p>
          ))}
        </section>
      )}
      {contact.organization && (
        <section data-contact-section="organization">
          <h3>Organization</h3>
          <p>{contact.organization}</p>
        </section>
      )}
      {contact.title && (
        <section data-contact-section="title">
          <h3>Title</h3>
          <p>{contact.title}</p>
        </section>
      )}
      {contact.birthday && (
        <section data-contact-section="birthday">
          <h3>Birthday</h3>
          <p>
            {contact.birthday.month}/{contact.birthday.day}
            {contact.birthday.year && `/${contact.birthday.year}`}
          </p>
        </section>
      )}
      {contact.notes && (
        <section data-contact-section="notes">
          <h3>Notes</h3>
          <p>{contact.notes}</p>
        </section>
      )}
    </>
  );
}

function Restore({
  list,
  canRestore,
  onClose,
  onRestore,
}: {
  list: RestorableContact[];
  onClose(): void;
  canRestore: boolean;
  onRestore(item: RestorableContact): void;
}) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => { closeRef.current?.focus(); }, []);

  return (
    <section
      id="contacts-restore-panel"
      role="dialog"
      aria-modal="true"
      aria-label="Recently deleted contacts"
    >
      <header>
        <button
          ref={closeRef}
          data-action="close-restore"
          aria-label="Close recently deleted"
          onClick={onClose}
        >
          ←
        </button>
        <h2>Recently deleted</h2>
      </header>
      {list.length ? (
        <ul id="contacts-restore-list">
          {list.map(item => (
            <li key={item.id} data-contact-id={item.id}>
              <ContactAvatar
                name={item.displayName}
                photoDataUrl={item.photoDataUrl}
              />
              <span>
                {item.displayName}
                <small>Deleted {item.deletedAt}</small>
              </span>
              {canRestore && (
                <button
                  data-action="restore-contact"
                  aria-label={`Restore ${item.displayName}`}
                  onClick={() => onRestore(item)}
                >
                  Restore
                </button>
              )}
            </li>
          ))}
        </ul>
      ) : (
        <p id="contacts-restore-empty">No contacts deleted in the past 90 days.</p>
      )}
      {list.length > 0 && !canRestore && (
        <p id="contacts-restore-owner-only">This book doesn't accept changes from this computer, so deleted contacts can't be restored from here.</p>
      )}
    </section>
  );
}
