import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HostedOnboarding } from "./HostedOnboarding";
import { bridge, type HostedPreviewView } from "./bridge";
import { peppyCopy } from "./generated/peppyCopy";

const view = (screen: string, extra: Partial<HostedPreviewView> = {}): HostedPreviewView => ({
  scenario: "new",
  screen,
  accountState: "signed_in",
  entitlementState: "active",
  approvalState: "none",
  unlocked: false,
  rejected: false,
  statusKey: null,
  localError: null,
  operationId: null,
  fixture: {
    accountLabel: "Preview account",
    signInProvider: null,
    subscription: { displayPrice: "$4.99/month · Preview price", status: "active" },
    approvalCode: "418 207",
    hostedOrigin: "preview.peppy.pro (preview)",
  },
  scenarios: ["new"],
  ...extra,
});

const renderView = (value: HostedPreviewView) => {
  vi.spyOn(bridge, "hosted_preview_state").mockResolvedValue(value);
  return render(<HostedOnboarding onSelfHosted={vi.fn()} />);
};

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("HostedOnboarding", () => {
  it("renders every screen id without a webview password input", async () => {
    const screens = [
      ["welcome", "onboarding-screen"],
      ["signin", "hosted-signin-screen"],
      ["subscribe", "subscribe-screen"],
      ["subscription_verifying", "subscribe-screen"],
      ["purchase_pending", "subscribe-screen"],
      ["passphrase", "passphrase-create-screen"],
      ["confirm", "passphrase-create-screen"],
      ["provisioning", "provisioning-screen"],
      ["join", "hosted-join-screen"],
      ["approval", "hosted-join-screen"],
      ["unlock", "hosted-unlock-screen"],
      ["permissions", "permissions-screen"],
      ["settings", "settings-server-section"],
      ["delete_account", "settings-server-section"],
      ["lapsed", "hosted-lapsed-screen"],
    ];

    for (const [screenName, id] of screens) {
      const { container, unmount } = renderView(view(screenName));
      await waitFor(() => expect(container.querySelector(`#${id}`)).toBeInTheDocument());
      expect(container.querySelector('input[type="password"]')).toBeNull();
      unmount();
      vi.restoreAllMocks();
    }
  });

  it("gates native passphrase entry and displays weak errors", async () => {
    const create = vi.spyOn(bridge, "hosted_preview_create_passphrase").mockResolvedValue(view("passphrase", { localError: "passphrase_weak" }));
    renderView(view("passphrase"));

    await screen.findByText(peppyCopy.passphrase_create_headline);
    expect(screen.getByRole("button", { name: peppyCopy.passphrase_native_cta })).toBeDisabled();
    fireEvent.click(screen.getByLabelText(peppyCopy.passphrase_ack_label));
    fireEvent.click(screen.getByRole("button", { name: peppyCopy.passphrase_native_cta }));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("alert")).toHaveTextContent(peppyCopy.passphrase_weak);
  });

  it("reports bridge errors and announces status copy", async () => {
    vi.spyOn(bridge, "hosted_preview_start").mockRejectedValue({ code: "preview-native-only" });
    renderView(view("welcome", { statusKey: "hosted_purchase_pending" }));

    await screen.findByText(peppyCopy.onboarding_headline);
    expect(document.getElementById("hosted-status-region")).toHaveTextContent(peppyCopy.hosted_purchase_pending);
    fireEvent.change(screen.getByLabelText(peppyCopy.preview_scenarios), { target: { value: "new" } });
    expect(await screen.findByRole("alert")).toHaveTextContent(peppyCopy.preview_error);
  });

  it("automatically verifies subscription checkpoints", async () => {
    const advance = vi.spyOn(bridge, "hosted_preview_advance").mockResolvedValue(view("subscribe"));
    renderView(view("subscription_verifying"));
    await waitFor(() => expect(advance).toHaveBeenCalledWith("verify_entitlement"));
  });

  it("automatically provisions again after a failed provisioning retry", async () => {
    const failed = view("provisioning", { statusKey: "provisioning_error" });
    const retrying = view("provisioning");
    const advance = vi.spyOn(bridge, "hosted_preview_advance")
      .mockResolvedValueOnce(failed)
      .mockResolvedValueOnce(retrying)
      .mockResolvedValueOnce(view("join"));
    renderView(view("provisioning"));

    await waitFor(() => expect(advance).toHaveBeenCalledWith("provision_finished"));
    expect(await screen.findByRole("button", { name: peppyCopy.try_again })).toHaveAttribute("id", "provisioning-retry");
    fireEvent.click(screen.getByRole("button", { name: peppyCopy.try_again }));
    await waitFor(() => expect(advance).toHaveBeenLastCalledWith("provision_finished"));
    expect(advance).toHaveBeenCalledTimes(3);
  });

  it("opens approval, advances Allow, and neutral Escape returns focus", async () => {
    const advance = vi.spyOn(bridge, "hosted_preview_advance")
      .mockResolvedValueOnce(view("approval"))
      .mockResolvedValueOnce(view("join"));
    renderView(view("join"));

    const approvalButton = await screen.findByRole("button", { name: peppyCopy.preview_approval });
    fireEvent.click(approvalButton);
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(advance).toHaveBeenCalledWith("show_approval");
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(advance).toHaveBeenCalledWith("back"));
    await waitFor(() => expect(document.activeElement).toBe(document.getElementById("hosted-screen-heading")));

    vi.restoreAllMocks();
    cleanup();
    const allowAdvance = vi.spyOn(bridge, "hosted_preview_advance")
      .mockResolvedValueOnce(view("approval"))
      .mockResolvedValueOnce(view("join"));
    renderView(view("join"));
    fireEvent.click(await screen.findByRole("button", { name: peppyCopy.preview_approval }));
    await screen.findByRole("dialog");
    fireEvent.click(screen.getByRole("button", { name: peppyCopy.device_allow }));
    await waitFor(() => expect(allowAdvance).toHaveBeenCalledWith("approval_granted"));
    await waitFor(() => expect(document.activeElement).toBe(document.getElementById("hosted-screen-heading")));
  });

  it("maps entitlement copy and keeps settings management local", async () => {
    const advance = vi.spyOn(bridge, "hosted_preview_advance");
    renderView(view("settings", { entitlementState: "grace", fixture: { ...view("settings").fixture, subscription: { displayPrice: "$4.99/month · Preview price", status: "grace" } } }));

    expect(await screen.findByText(peppyCopy.settings_server_grace)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: peppyCopy.settings_server_manage }));
    expect(screen.getByText(peppyCopy.settings_server_manage_preview)).toBeInTheDocument();
    expect(advance).not.toHaveBeenCalled();
  });
});
