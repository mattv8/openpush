import { useState } from "react";

/** Decorative contact portrait with a safe initials fallback. */
export function ContactAvatar({ photoDataUrl, name, size = 36, "data-contact-id": contactId }: {
  photoDataUrl?: string;
  name: string;
  size?: number;
  "data-contact-id"?: string;
}) {
  const [failed, setFailed] = useState(false);
  const initial = name.match(/[a-z0-9]/i)?.[0]?.toUpperCase() ?? "#";
  return <span className="contact-avatar" data-avatar-size={size} data-contact-id={contactId} aria-hidden="true" style={{ width: size, height: size, flex: `0 0 ${size}px` }}>
    {photoDataUrl && !failed ? <img src={photoDataUrl} alt="" width={size} height={size} onError={() => setFailed(true)} /> : <span style={{ fontSize: Math.round(size * .42) }}>{initial}</span>}
  </span>;
}
