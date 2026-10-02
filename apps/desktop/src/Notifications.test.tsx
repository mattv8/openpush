import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NotificationsView } from "./Notifications";
import type { MirroredNotification } from "./bridge";

const notification = (sourceDeviceId: string, packageName = "chat", overrides: Partial<MirroredNotification> = {}): MirroredNotification => ({
  target: { sourceDeviceId, notificationKey: `key-${sourceDeviceId}`, lifetime: "one" },
  packageName,
  appName: "Chat",
  title: `Title ${sourceDeviceId}`,
  text: "A readable notification body",
  postedAt: Date.now(),
  dismissible: true,
  seen: false,
  dismissalPending: false,
  ...overrides,
});

const props = (notifications: MirroredNotification[]) => ({
  notifications,
  filters: [],
  sources: [{ id: "phone-a", name: "Pixel" }, { id: "phone-b", name: "Tablet" }],
  locked: false,
  onDismiss: vi.fn(),
  onDismissAll: vi.fn(),
  onMute: vi.fn(),
  onSeen: vi.fn().mockResolvedValue(undefined),
  onSettings: vi.fn(),
});

afterEach(cleanup);

describe("NotificationsView", () => {
  it("keeps same-package apps isolated by phone and hides only the muted source", () => {
    const view = props([notification("phone-a"), notification("phone-b")]);
    const { rerender } = render(<NotificationsView {...view} />);
    expect(screen.getByLabelText("Notifications from Pixel")).toBeInTheDocument();
    expect(screen.getByLabelText("Notifications from Tablet")).toBeInTheDocument();
    rerender(<NotificationsView {...view} filters={[{ sourceDeviceId: "phone-a", packageName: "chat", appName: "Chat", muted: true }]} />);
    expect(screen.queryByLabelText("Notifications from Pixel")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Notifications from Tablet")).toBeInTheDocument();
  });

  it("keeps pending and non-dismissible notifications visible while disabling dismissal", () => {
    render(<NotificationsView {...props([notification("phone-a", "chat", { dismissalPending: true }), notification("phone-b", "mail", { dismissible: false })])} />);
    expect(screen.getByText(/Dismissal pending/)).toBeVisible();
    expect(screen.getByRole("button", { name: "Dismiss Title phone-a" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Dismiss Title phone-b" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Dismiss all" })).toBeDisabled();
  });

  it("uses a capped label only when more than one hundred dismissals are eligible", () => {
    const many = Array.from({ length: 101 }, (_, index) => notification(`phone-${index}`));
    render(<NotificationsView {...props(many)} />);
    expect(screen.getByRole("button", { name: "Dismiss up to 100" })).toBeEnabled();
  });

  it("marks rows seen after focus when they were already intersecting", async () => {
    let callback!: IntersectionObserverCallback;
    vi.stubGlobal("IntersectionObserver", class {
      constructor(next: IntersectionObserverCallback) { callback = next; }
      observe() {}
      disconnect() {}
      unobserve() {}
      takeRecords() { return []; }
      root = null;
      rootMargin = "0px";
      thresholds = [];
    });
    const view = props([notification("phone-a")]);
    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    render(<NotificationsView {...view} />);
    const row = screen.getByRole("article");
    callback([{ target: row, isIntersecting: true } as unknown as IntersectionObserverEntry], {} as IntersectionObserver);
    expect(view.onSeen).not.toHaveBeenCalled();
    vi.spyOn(document, "hasFocus").mockReturnValue(true);
    fireEvent.focus(window);
    await waitFor(() => expect(view.onSeen).toHaveBeenCalledTimes(1));
  });
});
