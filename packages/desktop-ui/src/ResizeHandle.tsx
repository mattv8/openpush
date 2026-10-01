import { useCallback, useEffect, useRef } from "react";
import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp } from "lucide-react";

export type ResizeHandleProps = {
  id?: string;
  direction: "horizontal" | "vertical";
  ariaLabel: string;
  value: number;
  min: number;
  max: number;
  valueUnit: "pixels" | "percent";
  onResize(delta: number): void;
  onResizeTo(value: number): void;
  className?: string;
  collapsed?: "before" | "after";
  collapsible?: { side: "before" | "after"; restoreValue: number };
  onResizeEnd?(): void;
  onResizeStart?(position: number): void;
  onDoubleClick?(): void;
};

/** Ported from Ragtime's workbench sash, without its segmented-bar theming. */
export function ResizeHandle({
  id,
  direction,
  ariaLabel,
  value,
  min,
  max,
  valueUnit,
  onResize,
  onResizeTo,
  className,
  collapsed,
  collapsible,
  onResizeEnd,
  onResizeStart,
  onDoubleClick,
}: ResizeHandleProps) {
  const startPos = useRef(0);
  const dragging = useRef(false);
  const moved = useRef(false);
  const pendingDelta = useRef(0);
  const frame = useRef<number | null>(null);
  const resize = useRef(onResize);
  const resizeEnd = useRef(onResizeEnd);
  resize.current = onResize;
  resizeEnd.current = onResizeEnd;

  const flush = useCallback(() => {
    frame.current = null;
    const delta = pendingDelta.current;
    pendingDelta.current = 0;
    if (delta) resize.current(delta);
  }, []);
  const finish = useCallback(() => {
    if (frame.current !== null || pendingDelta.current) flush();
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    if (dragging.current) resizeEnd.current?.();
    dragging.current = false;
  }, [flush]);
  useEffect(() => () => {
    if (frame.current !== null) window.cancelAnimationFrame(frame.current);
    finish();
  }, []);

  const isCollapsed = Boolean(collapsed);
  const keydown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const step = event.shiftKey ? 32 : 8;
    let handled = true;
    if (direction === "horizontal" && event.key === "ArrowLeft") onResize(-step);
    else if (direction === "horizontal" && event.key === "ArrowRight") onResize(step);
    else if (direction === "vertical" && event.key === "ArrowUp") onResize(-step);
    else if (direction === "vertical" && event.key === "ArrowDown") onResize(step);
    else if (event.key === "Home") onResizeTo(min);
    else if (event.key === "End") onResizeTo(max);
    else if (event.key === "Enter" && collapsible)
      onResizeTo(isCollapsed ? collapsible.restoreValue : 0);
    else handled = false;
    if (handled) {
      event.preventDefault();
      event.stopPropagation();
      onResizeEnd?.();
    }
  };
  const Icon = isCollapsed
    ? direction === "horizontal"
      ? collapsed === "before" ? ChevronRight : ChevronLeft
      : collapsed === "before" ? ChevronDown : ChevronUp
    : null;
  const cls = className ?? `resize-handle resize-handle-${direction}`;
  return (
    <div
      id={id}
      className={isCollapsed ? `${cls} resize-handle-collapsed` : cls}
      role="separator"
      aria-label={ariaLabel}
      aria-orientation={direction === "horizontal" ? "vertical" : "horizontal"}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={isCollapsed ? 0 : value}
      aria-valuetext={isCollapsed ? "Collapsed" : `${Math.round(value)} ${valueUnit}`}
      tabIndex={0}
      title={isCollapsed ? "Drag, click, or press Enter to restore pane" : undefined}
      data-value-unit={valueUnit}
      data-collapsed-side={collapsed}
      onKeyDown={keydown}
      onDoubleClick={onDoubleClick}
      onPointerDown={(event) => {
        event.preventDefault();
        const position = direction === "horizontal" ? event.clientX : event.clientY;
        onResizeStart?.(position);
        event.currentTarget.setPointerCapture(event.pointerId);
        startPos.current = position;
        moved.current = false;
        document.body.style.cursor =
          direction === "horizontal" ? "col-resize" : "row-resize";
        document.body.style.userSelect = "none";
        dragging.current = true;
      }}
      onPointerMove={(event) => {
        if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
        const position = direction === "horizontal" ? event.clientX : event.clientY;
        const delta = position - startPos.current;
        pendingDelta.current += delta;
        startPos.current = position;
        if (delta) moved.current = true;
        if (frame.current === null) frame.current = window.requestAnimationFrame(flush);
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId))
          event.currentTarget.releasePointerCapture(event.pointerId);
        if (isCollapsed && !moved.current && collapsible)
          onResizeTo(collapsible.restoreValue);
        finish();
      }}
      onPointerCancel={finish}
    >
      <span className="resize-handle-grip" aria-hidden="true">
        <span className="resize-handle-grip-dot" />
        <span className="resize-handle-grip-dot" />
        <span className="resize-handle-grip-dot" />
      </span>
      {Icon && <Icon size={14} className="resize-handle-chevron" />}
    </div>
  );
}
