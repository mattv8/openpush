export const OVERLAY_SCROLL_HIDE_DELAY = 1000;

export function installOverlayScrollbars(root: Document | HTMLElement): () => void {
  const timers = new Map<Element, ReturnType<typeof setTimeout>>();
  const scrollingElements = new Set<Element>();

  const clearScrolling = (element: Element) => {
    const timer = timers.get(element);
    if (timer) clearTimeout(timer);
    timers.delete(element);
    element.removeAttribute("data-scrolling");
    scrollingElements.delete(element);
  };

  const onScroll = (event: Event) => {
    let target = event.target;
    if (target instanceof Document) target = target.scrollingElement;
    if (!(target instanceof Element) || target.hasAttribute("data-scroll-programmatic")) {
      return;
    }

    const timer = timers.get(target);
    if (timer) clearTimeout(timer);
    target.setAttribute("data-scrolling", "true");
    scrollingElements.add(target);
    timers.set(
      target,
      setTimeout(() => clearScrolling(target), OVERLAY_SCROLL_HIDE_DELAY),
    );
  };

  root.addEventListener("scroll", onScroll, true);
  return () => {
    root.removeEventListener("scroll", onScroll, true);
    for (const timer of timers.values()) clearTimeout(timer);
    timers.clear();
    for (const element of scrollingElements) {
      element.removeAttribute("data-scrolling");
    }
    scrollingElements.clear();
  };
}
