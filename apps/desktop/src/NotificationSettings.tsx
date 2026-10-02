import { useState } from "react";
import type { AppFilter, MirroredNotification, NotificationPreferences } from "./bridge";
import { notificationTuple, notificationSourceName } from "./Notifications";

export function NotificationSettings({ notifications, filters, sources, preferences, onPreferences, onMute, onPermission }: {
  notifications: MirroredNotification[];
  filters: AppFilter[];
  sources: { id: string; name: string }[];
  preferences: NotificationPreferences;
  onPreferences(value: NotificationPreferences): Promise<void>;
  onMute(filter: AppFilter): Promise<void>;
  onPermission(): Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  // Observed app defaults never override authoritative synchronized mute choices.
  const apps = [...new Map([
    ...notifications.map(n => ({ sourceDeviceId: n.target.sourceDeviceId, packageName: n.packageName, appName: n.appName, muted: false })),
    ...filters,
  ].map(filter => [notificationTuple(filter.sourceDeviceId, filter.packageName), filter])).values()];
  const sourceIds = apps.map(app => app.sourceDeviceId);
  const save = async (action: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    setError("");
    try { await action(); }
    catch { setError("Could not save notification settings. Try again."); }
    finally { setBusy(false); }
  };
  return (
    <section id="notification-settings" data-settings-section="notifications" aria-busy={busy}>
      <h2>Notification mirroring</h2>
      <p>Mirrored app notifications use encrypted device sync. Carrier SMS/MMS has separate security boundaries.</p>
      <h3>Desktop banners</h3>
      <div className="settings-control-row">
        <label htmlFor="notification-message-banners">SMS/MMS banners</label>
        <input id="notification-message-banners" type="checkbox" role="switch"
          checked={preferences.messageBanners} aria-checked={preferences.messageBanners} disabled={busy}
          onChange={event => void save(() => onPreferences({ ...preferences, messageBanners: event.target.checked }))} />
      </div>
      <div className="settings-control-row">
        <label htmlFor="notification-mirrored-banners">App notification banners</label>
        <input id="notification-mirrored-banners" type="checkbox" role="switch"
          checked={preferences.mirroredBanners} aria-checked={preferences.mirroredBanners} disabled={busy}
          onChange={event => void save(() => onPreferences({ ...preferences, mirroredBanners: event.target.checked }))} />
      </div>
      <div className="settings-control-row">
        <label htmlFor="notification-preview">Banner preview</label>
        <select id="notification-preview" value={preferences.preview} disabled={busy}
          onChange={event => void save(() => onPreferences({ ...preferences, preview: event.target.value as NotificationPreferences["preview"] }))}>
          <option value="full">Full content</option>
          <option value="hidden">Hidden — generic banner only</option>
        </select>
      </div>
      <button className="secondary-button" disabled={busy} onClick={() => void save(onPermission)}>
        Check desktop notifications
      </button>
      <h3>Per-app mirroring</h3>
      <p>Apps are mirrored by default. Changes apply on the phone&apos;s next sync.</p>
      <p className="settings-note">Notification history shares the vault&apos;s 100,000-record recovery limit. Mute noisy apps to limit storage growth.</p>
      {error && <p role="alert">{error}</p>}
      <ul id="notification-app-filter-list" aria-label="App mirroring filters">
        {apps.map(filter => {
          const id = notificationTuple(filter.sourceDeviceId, filter.packageName);
          const phone = notificationSourceName(filter.sourceDeviceId, sources, sourceIds);
          return (
            <li key={id} data-device-id={filter.sourceDeviceId} data-package={filter.packageName} className="notif-app-filter-row">
              <span className="notif-app-letter-avatar" aria-hidden>{filter.appName.slice(0, 1).toUpperCase()}</span>
              <span className="notif-app-filter-label"><b>{filter.appName}</b><small>{phone}</small></span>
              <input id={`filter-${id}`} type="checkbox" role="switch" disabled={busy}
                aria-label={`Mirror ${filter.appName} from ${phone}`} checked={!filter.muted} aria-checked={!filter.muted}
                onChange={event => void save(() => onMute({ ...filter, muted: !event.target.checked }))} />
            </li>
          );
        })}
      </ul>
      {!apps.length && <p>No apps observed yet. Enable mirroring in the Android companion to begin.</p>}
    </section>
  );
}
