import { useEffect, useRef, useState } from "react";
import { ContactAvatar, X } from "@peppy/desktop-ui";
import {
  type ContactView,
  type ContactPhone,
  type ContactEmail,
  type ContactAddress,
  type ContactBirthday,
  bridge,
} from "./bridge";
import "./contact-form.css";

export interface ContactEditDraft {
  givenName?: string;
  familyName?: string;
  nickname?: string;
  organization?: string;
  title?: string;
  phones: ContactPhone[];
  emails: ContactEmail[];
  addresses: ContactAddress[];
  birthday?: ContactBirthday;
  notes?: string;
  photoKind: "keep" | "remove" | "set";
  photoDataUrl?: string;
}

type EditorDraft = Omit<ContactEditDraft, "phones" | "emails" | "addresses"> & {
  phones: (ContactPhone & { editorKey: string })[];
  emails: (ContactEmail & { editorKey: string })[];
  addresses: (ContactAddress & { editorKey: string })[];
};

/** Keeps an unknown wire label selectable (shown with its display text) so it round-trips. */
function LabelOptions({ label, displayLabel, options }: { label: string; displayLabel?: string; options: [string, string][] }) {
  const known = options.some(([value]) => value === label);
  return (
    <>
      {!known && <option value={label}>{displayLabel || label || "No label"}</option>}
      {options.map(([value, text]) => (
        <option key={value} value={value}>{text}</option>
      ))}
    </>
  );
}

const PHONE_LABELS: [string, string][] = [["mobile", "Mobile"], ["home", "Home"], ["work", "Work"], ["other", "Other"]];
const EMAIL_LABELS: [string, string][] = [["personal", "Personal"], ["work", "Work"], ["other", "Other"]];
const ADDRESS_LABELS: [string, string][] = [["home", "Home"], ["work", "Work"], ["other", "Other"]];

export function ContactForm({
  initial,
  onCancel,
  onSave,
  onDirtyChange,
  error,
  busy,
  bookCapabilities,
}: {
  initial?: ContactView;
  onCancel(): void;
  onSave(draft: ContactEditDraft): void;
  onDirtyChange?(dirty: boolean): void;
  error: string;
  busy?: boolean;
  bookCapabilities?: { canWrite: boolean; supportsNotes: boolean; supportsPhoto: boolean; supportsBirthday?: boolean };
}) {
  const rowSequence = useRef(0);
  // Existing domain IDs win; new unsaved rows get a stable form-local allocation ID.
  const rowKey = (kind: string, id?: string) => id ? `${kind}-stored-${id}` : `${kind}-draft-${++rowSequence.current}`;
  const [draft, setDraft] = useState<EditorDraft>(() =>
    initial
      ? {
          givenName: initial.givenName,
          familyName: initial.familyName,
          nickname: initial.nickname,
          organization: initial.organization,
          title: initial.title,
          phones: initial.phones.map(value => ({ ...value, editorKey: rowKey("phone", value.id) })),
          emails: initial.emails.map(value => ({ ...value, editorKey: rowKey("email", value.id) })),
          addresses: initial.addresses.map(value => ({ ...value, editorKey: rowKey("address", value.id) })),
          birthday: initial.birthday,
          notes: initial.notes,
          photoKind: "keep",
          photoDataUrl: initial.photoDataUrl,
        }
      : {
          phones: [],
          emails: [],
          addresses: [],
          photoKind: "keep",
        }
  );

  const initialDraftJson = useRef(JSON.stringify(draft));
  useEffect(() => {
    onDirtyChange?.(JSON.stringify(draft) !== initialDraftJson.current);
  }, [draft, onDirtyChange]);

  const [photoError, setPhotoError] = useState("");
  const [cropDialog, setCropDialog] = useState<{
    dataUrl: string;
    naturalWidth: number;
    naturalHeight: number;
  } | null>(null);

  const displayName =
    [draft.givenName, draft.familyName].filter(Boolean).join(" ") ||
    draft.nickname ||
    "#";

  const handlePhoneChange = (index: number, field: "label" | "number", value: string) => {
    setDraft(prev => ({
      ...prev,
      phones: prev.phones.map((p, i) =>
        i === index ? { ...p, [field]: value } : p
      ),
    }));
  };

  const handlePhoneAdd = () => {
    const editorKey = rowKey("phone");
    setDraft(prev => ({
      ...prev,
      phones: [...prev.phones, { label: "mobile", number: "", editorKey }],
    }));
  };

  const handlePhoneRemove = (index: number) => {
    setDraft(prev => ({
      ...prev,
      phones: prev.phones.filter((_, i) => i !== index),
    }));
  };

  const handleEmailChange = (index: number, field: "label" | "address", value: string) => {
    setDraft(prev => ({
      ...prev,
      emails: prev.emails.map((e, i) =>
        i === index ? { ...e, [field]: value } : e
      ),
    }));
  };

  const handleEmailAdd = () => {
    const editorKey = rowKey("email");
    setDraft(prev => ({
      ...prev,
      emails: [...prev.emails, { label: "personal", address: "", editorKey }],
    }));
  };

  const handleEmailRemove = (index: number) => {
    setDraft(prev => ({
      ...prev,
      emails: prev.emails.filter((_, i) => i !== index),
    }));
  };

  const handleAddressChange = (
    index: number,
    field: keyof ContactAddress,
    value: string
  ) => {
    setDraft(prev => ({
      ...prev,
      addresses: prev.addresses.map((a, i) =>
        i === index ? { ...a, [field]: value } : a
      ),
    }));
  };

  const handleAddressAdd = () => {
    const editorKey = rowKey("address");
    setDraft(prev => ({
      ...prev,
      addresses: [...prev.addresses, { label: "home", editorKey }],
    }));
  };

  const handleAddressRemove = (index: number) => {
    setDraft(prev => ({
      ...prev,
      addresses: prev.addresses.filter((_, i) => i !== index),
    }));
  };

  const handleBirthdayChange = (field: keyof ContactBirthday, value: string) => {
    const num = value ? parseInt(value, 10) : undefined;
    setDraft(prev => ({
      ...prev,
      birthday: {
        ...(prev.birthday ?? {}),
        [field]: num,
      },
    }));
  };

  const handlePhotoPickerClick = async () => {
    setPhotoError("");
    try {
      const picked = await bridge.pick_contact_photo();
      if (picked) setCropDialog(picked);
    } catch (e) {
      setPhotoError(String((e as { message?: string })?.message ?? e));
    }
  };

  const handleCropConfirm = (croppedDataUrl: string) => {
    setDraft(prev => ({
      ...prev,
      photoKind: "set",
      photoDataUrl: croppedDataUrl,
    }));
    setCropDialog(null);
  };

  const fieldReadOnly = bookCapabilities ? !bookCapabilities.canWrite : false;

  return (
    <section
      id="contact-edit-form"
      role="form"
      aria-label={initial ? "Edit contact" : "Create contact"}
      {...(busy ? { inert: true } : {})}
    >
      <header id="contact-form-header">
        <button type="button" data-action="cancel-edit" onClick={onCancel} disabled={busy}>
          Cancel
        </button>
        <h2>{initial ? "Edit contact" : "Create contact"}</h2>
        <button
          data-action="save-edit"
          className="primary-button"
          disabled={!displayName.trim() || displayName === "#" || busy || fieldReadOnly}
          onClick={() => onSave({
            ...draft,
            phones: draft.phones.map(({ editorKey: _key, ...value }) => value),
            emails: draft.emails.map(({ editorKey: _key, ...value }) => value),
            addresses: draft.addresses.map(({ editorKey: _key, ...value }) => value),
          })}
        >
          {busy ? "Saving..." : "Save"}
        </button>
      </header>
      {error && (
        <p role="alert" id="contact-form-error">
          {error}
        </p>
      )}
      <div id="contact-form-body">
        <section data-form-section="name">
          <label htmlFor="contact-form-given-name">Given name</label>
          <input
            id="contact-form-given-name"
            autoFocus
            readOnly={fieldReadOnly}
            value={draft.givenName ?? ""}
            onChange={e =>
              setDraft(prev => ({ ...prev, givenName: e.target.value }))
            }
          />
        </section>

        <section data-form-section="name">
          <label htmlFor="contact-form-family-name">Family name</label>
          <input
            id="contact-form-family-name"
            readOnly={fieldReadOnly}
            value={draft.familyName ?? ""}
            onChange={e =>
              setDraft(prev => ({ ...prev, familyName: e.target.value }))
            }
          />
        </section>

        <section data-form-section="name">
          <label htmlFor="contact-form-nickname">Nickname</label>
          <input
            id="contact-form-nickname"
            readOnly={fieldReadOnly}
            value={draft.nickname ?? ""}
            onChange={e =>
              setDraft(prev => ({ ...prev, nickname: e.target.value }))
            }
          />
        </section>

        {bookCapabilities?.supportsPhoto && (
          <section data-form-section="photo">
            <label htmlFor="contact-photo-picker">Photo</label>
            <button
              id="contact-photo-picker"
              data-action="pick-photo"
              aria-label={draft.photoDataUrl ? "Change photo" : "Add photo"}
              onClick={handlePhotoPickerClick}
              type="button"
            >
              <ContactAvatar name={displayName} photoDataUrl={draft.photoDataUrl} size={64} />
            </button>
            {/* Remove is distinct from keep: shown for a new crop or an existing photo. */}
            {draft.photoKind !== "remove" && (draft.photoDataUrl || initial?.photoPending) && !fieldReadOnly && (
              <div id="contact-photo-preview">
                <button
                  data-action="remove-photo"
                  aria-label="Remove photo"
                  onClick={() =>
                    setDraft(prev => ({
                      ...prev,
                      photoKind: "remove",
                      photoDataUrl: undefined,
                    }))
                  }
                  type="button"
                >
                  <X size={16} />
                </button>
              </div>
            )}
            <p id="contact-photo-hint">
              Photos are normalized to 256×256 before encrypted sync.
            </p>
            {photoError && (
              <p id="contact-photo-error" role="alert">
                {photoError}
              </p>
            )}
          </section>
        )}

        <section data-form-section="organization">
          <label htmlFor="contact-form-organization">Organization</label>
          <input
            id="contact-form-organization"
            readOnly={fieldReadOnly}
            value={draft.organization ?? ""}
            onChange={e =>
              setDraft(prev => ({ ...prev, organization: e.target.value }))
            }
          />
        </section>

        <section data-form-section="title">
          <label htmlFor="contact-form-title">Title</label>
          <input
            id="contact-form-title"
            readOnly={fieldReadOnly}
            value={draft.title ?? ""}
            onChange={e =>
              setDraft(prev => ({ ...prev, title: e.target.value }))
            }
          />
        </section>

        {bookCapabilities?.supportsBirthday && (
        <section data-form-section="birthday">
          <fieldset id="contact-form-birthday-fieldset">
            <legend>Birthday (optional)</legend>
            <div id="contact-form-birthday-inputs">
              <div>
                <label htmlFor="contact-form-birthday-month">Month</label>
                <input
                  id="contact-form-birthday-month"
                  type="number"
                  min="1"
                  max="12"
                  placeholder="MM"
                  readOnly={fieldReadOnly}
                  value={draft.birthday?.month ?? ""}
                  onChange={e => handleBirthdayChange("month", e.target.value)}
                />
              </div>
              <div>
                <label htmlFor="contact-form-birthday-day">Day</label>
                <input
                  id="contact-form-birthday-day"
                  type="number"
                  min="1"
                  max="31"
                  placeholder="DD"
                  readOnly={fieldReadOnly}
                  value={draft.birthday?.day ?? ""}
                  onChange={e => handleBirthdayChange("day", e.target.value)}
                />
              </div>
              <div>
                <label htmlFor="contact-form-birthday-year">Year</label>
                <input
                  id="contact-form-birthday-year"
                  type="number"
                  placeholder="YYYY"
                  readOnly={fieldReadOnly}
                  value={draft.birthday?.year ?? ""}
                  onChange={e => handleBirthdayChange("year", e.target.value)}
                />
              </div>
            </div>
          </fieldset>
        </section>
        )}

        <section data-form-section="phones">
          <header id="contact-form-phones-header">
            <h3>Phone numbers</h3>
            <button
              data-action="add-phone"
              hidden={fieldReadOnly}
              aria-label="Add phone number"
              onClick={handlePhoneAdd}
              type="button"
            >
              +
            </button>
          </header>
          {draft.phones.length > 0 ? (
            <ul id="contact-form-phones-list" role="list">
              {draft.phones.map((phone, index) => (
                <li key={phone.editorKey} data-contact-phone-row={phone.editorKey}>
                  <select
                    aria-label={`Phone ${index + 1} label`}
                    value={phone.label}
                    disabled={fieldReadOnly || phone.readOnly}
                    onChange={e =>
                      handlePhoneChange(index, "label", e.target.value)
                    }
                  >
                    <LabelOptions label={phone.label} displayLabel={phone.displayLabel} options={PHONE_LABELS} />
                  </select>
                  <input
                    type="tel"
                    aria-label={`Phone ${index + 1} number`}
                    placeholder="Phone number"
                    readOnly={fieldReadOnly || phone.readOnly}
                    value={phone.number}
                    onChange={e =>
                      handlePhoneChange(index, "number", e.target.value)
                    }
                  />
                  <button
                    data-action="remove-phone"
                    aria-label={`Remove phone ${index + 1}`}
                    disabled={fieldReadOnly || phone.readOnly}
                    onClick={() => handlePhoneRemove(index)}
                    type="button"
                  >
                    <X size={16} />
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <p id="contact-form-phones-empty">No phone numbers added yet</p>
          )}
        </section>

        <section data-form-section="emails">
          <header id="contact-form-emails-header">
            <h3>Email addresses</h3>
            <button
              data-action="add-email"
              hidden={fieldReadOnly}
              aria-label="Add email address"
              onClick={handleEmailAdd}
              type="button"
            >
              +
            </button>
          </header>
          {draft.emails.length > 0 ? (
            <ul id="contact-form-emails-list" role="list">
              {draft.emails.map((email, index) => (
                <li key={email.editorKey} data-contact-email-row={email.editorKey}>
                  <select
                    aria-label={`Email ${index + 1} label`}
                    value={email.label}
                    disabled={fieldReadOnly || email.readOnly}
                    onChange={e =>
                      handleEmailChange(index, "label", e.target.value)
                    }
                  >
                    <LabelOptions label={email.label} displayLabel={email.displayLabel} options={EMAIL_LABELS} />
                  </select>
                  <input
                    type="email"
                    aria-label={`Email ${index + 1} address`}
                    placeholder="Email address"
                    readOnly={fieldReadOnly || email.readOnly}
                    value={email.address}
                    onChange={e =>
                      handleEmailChange(index, "address", e.target.value)
                    }
                  />
                  <button
                    data-action="remove-email"
                    aria-label={`Remove email ${index + 1}`}
                    disabled={fieldReadOnly || email.readOnly}
                    onClick={() => handleEmailRemove(index)}
                    type="button"
                  >
                    <X size={16} />
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <p id="contact-form-emails-empty">No email addresses added yet</p>
          )}
        </section>

        <section data-form-section="addresses">
          <header id="contact-form-addresses-header">
            <h3>Addresses</h3>
            <button
              data-action="add-address"
              hidden={fieldReadOnly}
              aria-label="Add address"
              onClick={handleAddressAdd}
              type="button"
            >
              +
            </button>
          </header>
          {draft.addresses.length > 0 ? (
            <ul id="contact-form-addresses-list" role="list">
              {draft.addresses.map((address, index) => (
                <li key={address.editorKey} data-contact-address-row={address.editorKey}>
                  <select
                    aria-label={`Address ${index + 1} label`}
                    value={address.label}
                    disabled={fieldReadOnly || address.readOnly}
                    onChange={e =>
                      handleAddressChange(index, "label", e.target.value)
                    }
                  >
                    <LabelOptions label={address.label} displayLabel={address.displayLabel} options={ADDRESS_LABELS} />
                  </select>
                  <input
                    aria-label={`Address ${index + 1} street`}
                    placeholder="Street"
                    readOnly={fieldReadOnly || address.readOnly}
                    value={address.street ?? ""}
                    onChange={e =>
                      handleAddressChange(index, "street", e.target.value)
                    }
                  />
                  <input
                    aria-label={`Address ${index + 1} city`}
                    placeholder="City"
                    readOnly={fieldReadOnly || address.readOnly}
                    value={address.city ?? ""}
                    onChange={e =>
                      handleAddressChange(index, "city", e.target.value)
                    }
                  />
                  <input
                    aria-label={`Address ${index + 1} state`}
                    placeholder="State"
                    readOnly={fieldReadOnly || address.readOnly}
                    value={address.state ?? ""}
                    onChange={e =>
                      handleAddressChange(index, "state", e.target.value)
                    }
                  />
                  <input
                    aria-label={`Address ${index + 1} postal code`}
                    placeholder="Postal code"
                    readOnly={fieldReadOnly || address.readOnly}
                    value={address.postalCode ?? ""}
                    onChange={e =>
                      handleAddressChange(index, "postalCode", e.target.value)
                    }
                  />
                  <input
                    aria-label={`Address ${index + 1} country`}
                    placeholder="Country"
                    readOnly={fieldReadOnly || address.readOnly}
                    value={address.country ?? ""}
                    onChange={e =>
                      handleAddressChange(index, "country", e.target.value)
                    }
                  />
                  <button
                    data-action="remove-address"
                    aria-label={`Remove address ${index + 1}`}
                    disabled={fieldReadOnly || address.readOnly}
                    onClick={() => handleAddressRemove(index)}
                    type="button"
                  >
                    <X size={16} />
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <p id="contact-form-addresses-empty">No addresses added yet</p>
          )}
        </section>

        {bookCapabilities?.supportsNotes && (
          <section data-form-section="notes">
            <label htmlFor="contact-form-notes">Notes</label>
            <textarea
              id="contact-form-notes"
              readOnly={fieldReadOnly}
              value={draft.notes ?? ""}
              onChange={e =>
                setDraft(prev => ({ ...prev, notes: e.target.value }))
              }
              placeholder="Add notes"
              rows={4}
            />
          </section>
        )}
      </div>

      {cropDialog && (
        <PhotoCropDialog
          photo={cropDialog}
          onConfirm={handleCropConfirm}
          onCancel={() => setCropDialog(null)}
        />
      )}
    </section>
  );
}

function PhotoCropDialog({
  photo,
  onConfirm,
  onCancel,
}: {
  photo: { dataUrl: string; naturalWidth: number; naturalHeight: number };
  onConfirm(croppedDataUrl: string): void;
  onCancel(): void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState(false);
  const dragStart = useRef({ x: 0, y: 0 });

  const targetSize = 256;
  const minDim = Math.min(photo.naturalWidth, photo.naturalHeight);
  const scale = targetSize / minDim;
  const cropSize = minDim;

  const handleMouseDown = (e: React.MouseEvent) => {
    setIsDragging(true);
    dragStart.current = { x: e.clientX - offset.x, y: e.clientY - offset.y };
  };

  const handleMouseMove = (e: React.MouseEvent) => {
    if (!isDragging) return;
    const newX = e.clientX - dragStart.current.x;
    const newY = e.clientY - dragStart.current.y;
    const maxX = Math.max(0, (photo.naturalWidth - cropSize) * scale);
    const maxY = Math.max(0, (photo.naturalHeight - cropSize) * scale);
    setOffset({
      x: Math.max(0, Math.min(newX, maxX)),
      y: Math.max(0, Math.min(newY, maxY)),
    });
  };

  const handleMouseUp = () => {
    setIsDragging(false);
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    const step = e.shiftKey ? 32 : 8;
    const maxX = Math.max(0, (photo.naturalWidth - cropSize) * scale);
    const maxY = Math.max(0, (photo.naturalHeight - cropSize) * scale);

    if (e.key === "ArrowUp") {
      e.preventDefault();
      setOffset(prev => ({
        ...prev,
        y: Math.max(0, prev.y - step),
      }));
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setOffset(prev => ({
        ...prev,
        y: Math.min(maxY, prev.y + step),
      }));
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      setOffset(prev => ({
        ...prev,
        x: Math.max(0, prev.x - step),
      }));
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      setOffset(prev => ({
        ...prev,
        x: Math.min(maxX, prev.x + step),
      }));
    }
  };

  useEffect(() => {
    const img = new Image();
    img.onload = () => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;

      canvas.width = targetSize;
      canvas.height = targetSize;

      ctx.drawImage(
        img,
        offset.x / scale,
        offset.y / scale,
        cropSize,
        cropSize,
        0,
        0,
        targetSize,
        targetSize
      );
    };
    img.src = photo.dataUrl;
  }, [photo.dataUrl, offset, scale, cropSize, targetSize]);

  const handleConfirm = () => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const jpeg = canvas.toDataURL("image/jpeg", 0.85);
    onConfirm(jpeg);
  };

  return (
    <dialog id="photo-crop-dialog" aria-labelledby="photo-crop-title" open>
      <div id="photo-crop-container">
        <h3 id="photo-crop-title">Crop photo to square</h3>
        <div
          id="photo-crop-viewport"
          onMouseDown={handleMouseDown}
          onMouseMove={handleMouseMove}
          onMouseUp={handleMouseUp}
          onMouseLeave={handleMouseUp}
          onKeyDown={handleKeyDown}
          tabIndex={0}
          aria-label="Photo crop position"
          aria-describedby="photo-crop-hint"
        >
          <img
            id="photo-crop-image"
            src={photo.dataUrl}
            alt="Original photo to crop"
            style={{
              width: photo.naturalWidth * scale,
              height: photo.naturalHeight * scale,
              transform: `translate(-${offset.x}px, -${offset.y}px)`,
            }}
          />
        </div>
        <canvas id="photo-crop-preview" ref={canvasRef} width={targetSize} height={targetSize} />
        <p id="photo-crop-hint">Use arrow keys or drag to position. Shift+arrow moves faster.</p>
        <div id="photo-crop-actions">
          <button onClick={onCancel}>Cancel</button>
          <button className="primary-button" onClick={handleConfirm}>
            Confirm crop
          </button>
        </div>
      </div>
    </dialog>
  );
}
