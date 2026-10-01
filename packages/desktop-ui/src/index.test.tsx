import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AppTitlebar, Composer, RecipientPicker, ResizeHandle } from "./index";

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

  it("never renders the simulated badge", () => {
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
    expect(screen.queryByText("SIMULATED UI")).not.toBeInTheDocument();
  });

  it("provides macOS traffic lights and keyboard resizing", () => {
    const resize = vi.fn();
    const resizeTo = vi.fn();
    render(<main data-platform="macos"><AppTitlebar platform="macos" onMinimize={() => {}} onMaximize={() => {}} onClose={() => {}} /><ResizeHandle direction="horizontal" ariaLabel="Resize list" value={280} min={200} max={480} valueUnit="pixels" onResize={resize} onResizeTo={resizeTo} collapsible={{ side: "before", restoreValue: 280 }} /></main>);
    expect(screen.getByRole("button", { name: "Close window" })).toHaveClass("traffic-light");
    const handle = screen.getByRole("separator");
    fireEvent.keyDown(handle, { key: "ArrowLeft" });
    fireEvent.keyDown(handle, { key: "ArrowRight", shiftKey: true });
    fireEvent.keyDown(handle, { key: "Home" });
    fireEvent.keyDown(handle, { key: "Enter" });
    expect(resize).toHaveBeenNthCalledWith(1, -8);
    expect(resize).toHaveBeenNthCalledWith(2, 32);
    expect(resizeTo).toHaveBeenCalledWith(200);
    expect(resizeTo).toHaveBeenCalledWith(0);
  });

  it("does not end a drag when its parent rerenders", () => {
    const ended = vi.fn();
    const resize = vi.fn();
    const { rerender } = render(
      <ResizeHandle
        direction="horizontal"
        ariaLabel="Resize list"
        value={280}
        min={200}
        max={480}
        valueUnit="pixels"
        onResize={resize}
        onResizeTo={() => {}}
        onResizeEnd={ended}
      />,
    );
    const handle = screen.getByRole("separator") as HTMLDivElement;
    handle.setPointerCapture = vi.fn();
    handle.hasPointerCapture = vi.fn(() => true);
    handle.releasePointerCapture = vi.fn();
    fireEvent.pointerDown(handle, { pointerId: 1, clientX: 100 });
    rerender(
      <ResizeHandle
        direction="horizontal"
        ariaLabel="Resize list"
        value={281}
        min={200}
        max={480}
        valueUnit="pixels"
        onResize={resize}
        onResizeTo={() => {}}
        onResizeEnd={() => ended()}
      />,
    );
    expect(ended).not.toHaveBeenCalled();
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 101 });
    expect(ended).toHaveBeenCalledOnce();
  });

  it("counts SMS characters only as text approaches the limit", () => {
    const { rerender } = render(<Composer draft={"a".repeat(135)} attachments={[]} sendSupported onDraftChange={() => {}} onSend={() => {}} />);
    expect(screen.queryByLabelText("SMS character count")).not.toBeInTheDocument();
    rerender(<Composer draft={"a".repeat(161)} attachments={[]} sendSupported onDraftChange={() => {}} onSend={() => {}} />);
    expect(screen.getByLabelText("SMS character count")).toHaveTextContent("2 SMS · 145 left");
    rerender(<Composer draft={"😀".repeat(60)} attachments={[]} sendSupported onDraftChange={() => {}} onSend={() => {}} />);
    expect(screen.getByLabelText("SMS character count")).toHaveTextContent("2 SMS · 14 left");
    rerender(<Composer draft={"a".repeat(160)} attachments={[{ id: "a", name: "a.png", state: "ready" }]} sendSupported onDraftChange={() => {}} onSend={() => {}} />);
    expect(screen.queryByLabelText("SMS character count")).not.toBeInTheDocument();
  });
});
