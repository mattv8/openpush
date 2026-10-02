import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AppTitlebar,
  Composer,
  installOverlayScrollbars,
  isRecipientAnchor,
  RecipientPanel,
  RecipientPicker,
  ResizeHandle,
} from "./index";

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
    expect(document.querySelectorAll("#unavailable-hint")).toHaveLength(1);
    expect(screen.getByText("Sending unavailable: choose a gateway and SIM")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    rerender(
      <Composer
        draft="hi"
        attachments={[]}
        sendSupported={false}
        gatewaySlot={<span id="unavailable-hint">Gateway unavailable</span>}
        onDraftChange={() => {}}
        onSend={sent}
      />,
    );
    expect(document.querySelectorAll("#unavailable-hint")).toHaveLength(1);
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

  it("renders titlebar status only when provided", () => {
    const noop = () => {};
    const { rerender } = render(
      <AppTitlebar onMinimize={noop} onMaximize={noop} onClose={noop} status={<span>Status node</span>} />,
    );
    expect(document.getElementById("titlebar-status")).toHaveTextContent("Status node");
    rerender(<AppTitlebar onMinimize={noop} onMaximize={noop} onClose={noop} />);
    expect(document.getElementById("titlebar-status")).not.toBeInTheDocument();
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

  it("caps composer auto-grow at the configured height and defaults to 176px", () => {
    Object.defineProperty(HTMLTextAreaElement.prototype, "scrollHeight", {
      configurable: true,
      get: () => 240,
    });
    const props = {
      draft: "Message",
      attachments: [],
      sendSupported: true,
      onDraftChange: () => {},
      onSend: () => {},
    };
    const { rerender } = render(<Composer {...props} maxAutoGrowHeight={100} />);
    expect(screen.getByLabelText("Message")).toHaveStyle({ height: "100px" });

    rerender(<Composer {...props} />);
    expect(screen.getByLabelText("Message")).toHaveStyle({ height: "176px" });
  });

  it("tokenises recipient input only on a commit action", () => {
    const committed = vi.fn();
    render(<RecipientPanel recipients={[]} onCommit={committed} anchor="top-left" onAnchorChange={() => {}} />);
    const input = screen.getByLabelText("Recipients");
    fireEvent.change(input, { target: { value: "one" } });
    expect(committed).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "," });
    expect(committed).toHaveBeenCalledWith(["one"]);
  });

  it("commits recipient input on Enter, blur, and separated paste without duplicates", () => {
    const committed = vi.fn();
    const { rerender } = render(<RecipientPanel recipients={[{ id: "one", label: "One" }]} onCommit={committed} anchor="top-left" onAnchorChange={() => {}} />);
    const input = screen.getByLabelText("Recipients");
    fireEvent.change(input, { target: { value: "two;one" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(committed).toHaveBeenLastCalledWith(["one", "two"]);
    rerender(<RecipientPanel recipients={[{ id: "one", label: "One" }, { id: "two", label: "Two" }]} onCommit={committed} anchor="top-left" onAnchorChange={() => {}} />);
    fireEvent.change(input, { target: { value: "three" } });
    fireEvent.blur(input);
    expect(committed).toHaveBeenLastCalledWith(["one", "two", "three"]);
    fireEvent.paste(input, { clipboardData: { getData: () => "four, five" } });
    expect(committed).toHaveBeenLastCalledWith(["one", "two", "four"]);
    expect(input).toHaveValue("five");
  });

  it("removes recipient chips by Backspace and their remove button", () => {
    const committed = vi.fn();
    render(<RecipientPanel recipients={[{ id: "one", label: "One" }, { id: "two", label: "Two" }]} onCommit={committed} anchor="top-left" onAnchorChange={() => {}} />);
    const input = screen.getByLabelText("Recipients");
    fireEvent.keyDown(input, { key: "Backspace" });
    expect(committed).toHaveBeenLastCalledWith(["one"]);
    fireEvent.click(screen.getByRole("button", { name: "Remove One" }));
    expect(committed).toHaveBeenLastCalledWith(["two"]);
    expect(input).toHaveFocus();
    expect(screen.getByText("Group · MMS")).toBeInTheDocument();
  });

  it("moves recipient anchors with arrow keys while preserving focus and pending input", () => {
    const changed = vi.fn();
    const { rerender } = render(
      <Composer
        draft=""
        attachments={[]}
        sendSupported
        onDraftChange={() => {}}
        onSend={() => {}}
        recipientAnchor="top-left"
        recipientSlot={<RecipientPanel recipients={[]} onCommit={() => {}} anchor="top-left" onAnchorChange={changed} />}
      />,
    );
    const input = screen.getByLabelText("Recipients");
    const grip = screen.getByRole("button", { name: "Move recipients panel" });
    input.focus();
    fireEvent.change(input, { target: { value: "uncommitted" } });
    fireEvent.keyDown(grip, { key: "ArrowDown" });
    expect(changed).toHaveBeenCalledWith("bottom-left");
    rerender(
      <Composer
        draft=""
        attachments={[]}
        sendSupported
        onDraftChange={() => {}}
        onSend={() => {}}
        recipientAnchor="bottom-left"
        recipientSlot={<RecipientPanel recipients={[]} onCommit={() => {}} anchor="bottom-left" onAnchorChange={changed} />}
      />,
    );
    expect(input).toHaveFocus();
    expect(input).toHaveValue("uncommitted");
    expect(document.getElementById("composer-recipient-row")).toHaveAttribute("data-row", "bottom");
    expect(document.getElementById("composer-recipient-row")).toHaveAttribute("data-align", "start");
  });

  it("validates recipient anchors", () => {
    expect(isRecipientAnchor("top-left")).toBe(true);
    expect(isRecipientAnchor("center")).toBe(false);
  });
});

describe("installOverlayScrollbars", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows a thumb while scrolling and hides it after the final scroll", () => {
    vi.useFakeTimers();
    const target = document.createElement("div");
    document.body.append(target);
    const cleanup = installOverlayScrollbars(document);

    fireEvent.scroll(target);
    expect(target).toHaveAttribute("data-scrolling", "true");
    vi.advanceTimersByTime(999);
    expect(target).toHaveAttribute("data-scrolling", "true");
    vi.advanceTimersByTime(1);
    expect(target).not.toHaveAttribute("data-scrolling");

    cleanup();
    target.remove();
  });

  it("resets each target's hide timer after another scroll", () => {
    vi.useFakeTimers();
    const target = document.createElement("div");
    document.body.append(target);
    const cleanup = installOverlayScrollbars(document);

    fireEvent.scroll(target);
    vi.advanceTimersByTime(750);
    fireEvent.scroll(target);
    vi.advanceTimersByTime(250);
    expect(target).toHaveAttribute("data-scrolling", "true");
    vi.advanceTimersByTime(750);
    expect(target).not.toHaveAttribute("data-scrolling");

    cleanup();
    target.remove();
  });

  it("ignores programmatic scrolling", () => {
    const target = document.createElement("div");
    target.setAttribute("data-scroll-programmatic", "");
    document.body.append(target);
    const cleanup = installOverlayScrollbars(document);

    fireEvent.scroll(target);
    expect(target).not.toHaveAttribute("data-scrolling");

    cleanup();
    target.remove();
  });

  it("removes its listener, timers, and active attributes during cleanup", () => {
    vi.useFakeTimers();
    const target = document.createElement("div");
    document.body.append(target);
    const cleanup = installOverlayScrollbars(document);

    fireEvent.scroll(target);
    cleanup();
    expect(target).not.toHaveAttribute("data-scrolling");
    fireEvent.scroll(target);
    expect(target).not.toHaveAttribute("data-scrolling");

    target.remove();
  });
});
