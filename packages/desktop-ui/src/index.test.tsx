import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AppTitlebar, Composer, RecipientPicker } from "./index";

const people = [{ id: "conv-a", name: "Aurora", preview: "Hi", unread: 0 }];
afterEach(cleanup);

describe("desktop UI controls", () => {
  it("selects recipients with an ARIA combobox keyboard flow", () => {
    const changed = vi.fn();
    render(<RecipientPicker recipients={people} onChange={changed} />);
    const input = screen.getByRole("combobox");
    fireEvent.change(input, { target: { value: "Aur" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(changed).toHaveBeenCalledWith(["conv-a"]);
  });

  it("offers a new-recipient option for a typed address without a conversation", () => {
    const changed = vi.fn(),
      started = vi.fn();
    render(
      <RecipientPicker
        recipients={people}
        onChange={changed}
        onNewRecipient={started}
      />,
    );
    const input = screen.getByRole("combobox");
    fireEvent.change(input, { target: { value: " +1 555 0100 " } });
    expect(
      screen.getByRole("option", { name: "Message +1 555 0100" }),
    ).toBeInTheDocument();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(started).toHaveBeenCalledWith("+1 555 0100");
    expect(changed).not.toHaveBeenCalled();
  });

  it("does not commit a recipient during IME composition", () => {
    const changed = vi.fn();
    render(<RecipientPicker recipients={people} onChange={changed} />);
    const input = screen.getByRole("combobox");
    fireEvent.change(input, { target: { value: "Aur" } });
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(changed).not.toHaveBeenCalled();
  });

  it("does not send Enter during IME composition", () => {
    const sent = vi.fn();
    render(
      <Composer
        draft="こんにちは"
        attachments={[]}
        sendSupported
        onDraftChange={() => {}}
        onSend={sent}
      />,
    );
    fireEvent.keyDown(screen.getByLabelText("Message"), {
      key: "Enter",
      isComposing: true,
    });
    expect(sent).not.toHaveBeenCalled();
  });

  it("allows attachment-only sends and explains why sending is unavailable", () => {
    const sent = vi.fn();
    const { rerender } = render(
      <Composer
        draft=""
        attachments={[{ id: "a", name: "a.png", state: "ready" }]}
        sendSupported
        onDraftChange={() => {}}
        onSend={sent}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(sent).toHaveBeenCalledTimes(1);
    rerender(
      <Composer
        draft="hi"
        attachments={[]}
        sendSupported={false}
        unavailableReason="choose a gateway and SIM"
        onDraftChange={() => {}}
        onSend={sent}
      />,
    );
    expect(
      screen.getByText("Sending unavailable: choose a gateway and SIM"),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("shows the simulated badge only when asked", () => {
    const noop = () => {};
    const { rerender } = render(
      <AppTitlebar onMinimize={noop} onMaximize={noop} onClose={noop} />,
    );
    expect(screen.queryByText("SIMULATED UI")).not.toBeInTheDocument();
    rerender(
      <AppTitlebar
        onMinimize={noop}
        onMaximize={noop}
        onClose={noop}
        simulated
      />,
    );
    expect(screen.getByText("SIMULATED UI")).toBeInTheDocument();
  });
});
