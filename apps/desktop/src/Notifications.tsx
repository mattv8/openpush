import { useEffect, useRef } from "react";
import { Bell, Lock, X } from "@openpush/desktop-ui";
import type {
  AppFilter,
  MirroredNotification,
  NotificationTarget,
} from "./bridge";

type Source = { id: string; name: string };
type SeenResult = Promise<void>;

export const notificationTuple = (...parts: string[]) =>
  encodeURIComponent(JSON.stringify(parts));

export function notificationSourceName(id: string, sources: Source[], knownIds: string[]) {
  return sources.find(source => source.id === id)?.name ??
    `Phone ${[...new Set(knownIds)].sort().indexOf(id) + 1}`;
}

const relativeTime = (time: number) => {
  if (!Number.isFinite(time)) return "Unknown time";
  const minutes = Math.max(0, Math.round((Date.now() - time) / 60_000));
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 1440) return `${Math.round(minutes / 60)}h`;
  return `${Math.round(minutes / 1440)}d`;
};

const safeDateTime = (time: number) => {
  const date = new Date(time);
  return Number.isFinite(time) && !Number.isNaN(date.getTime())
    ? date.toISOString()
    : undefined;
};

export function NotificationsView({
  notifications,
  filters,
  sources,
  locked,
  onDismiss,
  onDismissAll,
  onMute,
  onSeen,
  onSettings,
}: {
  notifications: MirroredNotification[];
  filters: AppFilter[];
  sources: Source[];
  locked: boolean;
  onDismiss(target: NotificationTarget): void;
  onDismissAll(): void;
  onMute(filter: AppFilter): void;
  onSeen(targets: NotificationTarget[]): SeenResult;
  onSettings(): void;
}) {
  const feedRef = useRef<HTMLDivElement>(null);
  const visible = useRef(new Map<string, NotificationTarget>());
  const seen = useRef(new Set<string>());
  const inFlight = useRef(new Set<string>());
  const onSeenRef = useRef(onSeen);
  onSeenRef.current = onSeen;
  const isMuted = (notification: MirroredNotification) =>
    filters.some(
      (filter) =>
        filter.sourceDeviceId === notification.target.sourceDeviceId &&
        filter.packageName === notification.packageName &&
        filter.muted,
    );
  const shown = notifications.filter((notification) => !isMuted(notification));
  const shownKey = shown
    .map((notification) =>
      notificationTuple(
        notification.target.sourceDeviceId,
        notification.target.notificationKey,
        notification.target.lifetime,
      ),
    )
    .join(",");

  useEffect(() => {
    const flushSeen = () => {
      if (!document.hasFocus() || document.visibilityState !== "visible") return;
      const targets = [...visible.current.entries()]
        .filter(([key]) => !seen.current.has(key) && !inFlight.current.has(key))
        .map(([, target]) => target);
      if (!targets.length) return;
      const keys = targets.map((target) =>
        notificationTuple(
          target.sourceDeviceId,
          target.notificationKey,
          target.lifetime,
        ),
      );
      keys.forEach((key) => inFlight.current.add(key));
      void onSeenRef.current(targets).then(
        () => keys.forEach((key) => seen.current.add(key)),
        () => undefined,
      ).finally(() => keys.forEach((key) => inFlight.current.delete(key)));
    };
    const feed = feedRef.current;
    if (!feed || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          const row = entry.target as HTMLElement;
          const target = JSON.parse(
            row.dataset.notificationTarget ?? "null",
          ) as NotificationTarget | null;
          if (!target) return;
          const key = notificationTuple(
            target.sourceDeviceId,
            target.notificationKey,
            target.lifetime,
          );
          if (entry.isIntersecting) visible.current.set(key, target);
          else visible.current.delete(key);
        });
        flushSeen();
      },
      { root: feed },
    );
    feed
      .querySelectorAll<HTMLElement>("[data-notification-target]")
      .forEach((row) => observer.observe(row));
    window.addEventListener("focus", flushSeen);
    document.addEventListener("visibilitychange", flushSeen);
    flushSeen();
    return () => {
      observer.disconnect();
      window.removeEventListener("focus", flushSeen);
      document.removeEventListener("visibilitychange", flushSeen);
    };
  }, [shownKey]);

  if (locked) {
    return <section id="notifications-view" role="region" aria-label="Notifications">
      <div id="notifications-locked-state" role="status">
        <Lock size={32} aria-hidden />
        <p>Sync is locked</p>
        <p className="setup-hint">Unlock sync to receive notifications.</p>
      </div>
    </section>;
  }

  const dismissible = shown.filter(
    (notification) => notification.dismissible && !notification.dismissalPending,
  );
  const dismissAll = dismissible.length > 100 ? "Dismiss up to 100" : "Dismiss all";
  const toolbar = <header id="notifications-header" role="toolbar" aria-label="Notifications toolbar">
    <button id="notifications-dismiss-all" disabled={!dismissible.length} onClick={onDismissAll}>
      {dismissAll}
    </button>
  </header>;

  if (!shown.length) {
    return <section id="notifications-view" role="region" aria-label="Notifications">
      {toolbar}
      <div id="notifications-empty-state" role="status">
        <Bell size={32} aria-hidden />
        <p>No notifications</p>
        <p className="setup-hint">
          Enable notification mirroring and notification access in the Android
          companion.
        </p>
        <button
          className="secondary-button"
          onClick={onSettings}
        >
          Open settings
        </button>
      </div>
    </section>;
  }

  const phoneGroups = new Map<string, MirroredNotification[]>();
  shown.forEach((notification) => {
    const key = notification.target.sourceDeviceId;
    phoneGroups.set(key, [...(phoneGroups.get(key) ?? []), notification]);
  });
  return <section id="notifications-view" role="region" aria-label="Notifications">
    {toolbar}
    <div id="notifications-feed" ref={feedRef} role="feed" aria-label="Notification feed">
      {[...phoneGroups].map(([sourceDeviceId, phoneNotifications]) => {
        const phoneName = notificationSourceName(sourceDeviceId, sources, [...phoneGroups.keys()]);
        const apps = new Map<string, MirroredNotification[]>();
        phoneNotifications.forEach((notification) => {
          apps.set(notification.packageName, [...(apps.get(notification.packageName) ?? []), notification]);
        });
        return <section key={sourceDeviceId} id={`notif-phone-${notificationTuple(sourceDeviceId)}`} data-device-id={sourceDeviceId} className="notif-phone-group" aria-label={`Notifications from ${phoneName}`}>
          <header className="notif-group-header">
            <span className="avatar" aria-hidden>{phoneName.slice(0, 1).toUpperCase()}</span>
            <b className="notif-group-name">{phoneName}</b>
            <span className="notif-group-count">{apps.size} apps</span>
          </header>
          {[...apps].map(([packageName, appNotifications]) => {
            const app = appNotifications[0];
            const filter = filters.find((item) => item.sourceDeviceId === sourceDeviceId && item.packageName === packageName) ?? { sourceDeviceId, packageName, appName: app.appName, muted: false };
            return <section key={packageName} id={`notif-app-${notificationTuple(sourceDeviceId, packageName)}`} data-device-id={sourceDeviceId} data-package={packageName} className="notif-app-group" aria-label={`${app.appName} on ${phoneName}`}>
              <header className="notif-app-header">
                <span className="notif-app-letter-avatar" aria-hidden>{app.appName.slice(0, 1).toUpperCase()}</span>
                <b className="notif-app-name">{app.appName}</b>
                <span className="notif-app-count">{appNotifications.length}</span>
                <button className="notif-inline-action" aria-label={`Mute ${app.appName} on ${phoneName}`} onClick={() => onMute(filter)}>Mute</button>
              </header>
              {appNotifications.map((notification) => <NotificationRow key={notificationTuple(notification.target.sourceDeviceId, notification.target.notificationKey, notification.target.lifetime)} notification={notification} onDismiss={onDismiss} />)}
            </section>;
          })}
        </section>;
      })}
    </div>
  </section>;
}

function NotificationRow({ notification, onDismiss }: { notification: MirroredNotification; onDismiss(target: NotificationTarget): void }) {
  const pending = notification.dismissalPending;
  const id = notificationTuple(notification.target.sourceDeviceId, notification.target.notificationKey, notification.target.lifetime);
  return <article id={`notif-${id}`} data-device-id={notification.target.sourceDeviceId} data-notification-key={notification.target.notificationKey} data-dismiss-state={pending ? "pending" : "idle"} data-notification-target={JSON.stringify(notification.target)} className="notif-row" aria-label={`${notification.title}: ${notification.text}`}>
    <div className="notif-row-body">
      <b className="notif-row-title">{notification.title}</b>
      <span className="notif-row-text">{notification.text}</span>
      {pending && <span className="notif-pending" role="status">Dismissal pending — will apply on your phone&apos;s next sync.</span>}
    </div>
    <time className="notif-row-time" dateTime={safeDateTime(notification.postedAt)}>{relativeTime(notification.postedAt)}</time>
    <button className="notif-dismiss-btn" disabled={pending || !notification.dismissible} title={pending ? "Dismissal pending — will apply on your phone's next sync." : "Dismiss"} aria-label={`Dismiss ${notification.title || notification.appName || "notification"}`} onClick={() => onDismiss(notification.target)}>
      <X size={14} aria-hidden />
    </button>
  </article>;
}
