import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { NotificationSettings } from "./NotificationSettings";
import type { AppFilter, MirroredNotification, NotificationPreferences } from "./bridge";

afterEach(cleanup);
const preferences: NotificationPreferences = { messageBanners: true, mirroredBanners: true, preview: "full" };
const filter: AppFilter = { sourceDeviceId: "one", packageName: "chat", appName: "Chat", muted: true };
const notification: MirroredNotification = {
  target: { sourceDeviceId: "one", notificationKey: "key", lifetime: "life" },
  packageName: "chat", appName: "Chat", title: "Hello", text: "Text", postedAt: 0,
  dismissible: true, seen: false, dismissalPending: false,
};
const props = () => ({
  notifications: [notification], filters: [filter], sources: [{ id: "one", name: "Pixel" }],
  preferences, onPreferences: vi.fn().mockResolvedValue(undefined),
  onMute: vi.fn().mockResolvedValue(undefined), onPermission: vi.fn().mockResolvedValue(undefined),
});

it("keeps the stored mute choice over observed defaults and allows unmuting that phone", async () => {
  const input = props();
  render(<NotificationSettings {...input} />);
  const control = screen.getByRole("switch", { name: "Mirror Chat from Pixel" });
  expect(control).not.toBeChecked();
  fireEvent.click(control);
  await waitFor(() => expect(input.onMute).toHaveBeenCalledWith({ ...filter, muted: false }));
});

it("disables preferences while saving so a second click cannot submit a stale snapshot", async () => {
  let resolve!: () => void;
  const input = props();
  input.onPreferences.mockImplementation(() => new Promise<void>(done => { resolve = done; }));
  const { rerender } = render(<NotificationSettings {...input} />);
  fireEvent.click(screen.getByRole("switch", { name: "SMS/MMS banners" }));
  expect(input.onPreferences).toHaveBeenCalledWith({ ...preferences, messageBanners: false });
  expect(screen.getByRole("switch", { name: "App notification banners" })).toBeDisabled();
  expect(screen.getByLabelText("Banner preview")).toBeDisabled();
  rerender(<NotificationSettings {...input} preferences={{ ...preferences, messageBanners: false }} />);
  await act(async () => resolve());
  expect(screen.getByRole("switch", { name: "SMS/MMS banners" })).not.toBeChecked();
  expect(screen.getByLabelText("Banner preview")).toBeEnabled();
});

it("keeps unknown phones distinguishable and reports save failures", async () => {
  const input = props();
  input.onMute.mockRejectedValue(new Error("failure"));
  render(<NotificationSettings {...input} sources={[]} filters={[filter, { ...filter, sourceDeviceId: "two" }]} />);
  expect(screen.getByRole("switch", { name: "Mirror Chat from Phone 2" })).not.toBeChecked();
  fireEvent.click(screen.getByRole("switch", { name: "Mirror Chat from Phone 1" }));
  expect(await screen.findByRole("alert")).toBeVisible();
  expect(screen.getByRole("switch", { name: "Mirror Chat from Phone 1" })).toBeEnabled();
});
