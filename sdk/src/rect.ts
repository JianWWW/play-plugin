import type { Rect } from "./protocol";

/**
 * Computes the placeholder element's rectangle in **physical** screen pixels —
 * the coordinate space the native overlay window uses.
 *
 * Strategy: `window.screenX/screenY` (CSS px, relative to the virtual screen)
 * + Chromium's UI chrome offsets + the element's viewport rect, all scaled by
 * `devicePixelRatio` (which also tracks browser zoom).
 */
export function computeScreenRect(el: HTMLElement): Rect {
  const box = el.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;

  // Chromium: side chrome = (outer - inner) / 2; top chrome = outer - inner
  // (bottom is zero). Firefox matches closely enough; F11 fullscreen gives 0.
  const chromeSide = (window.outerWidth - window.innerWidth) / 2;
  const chromeTop = window.outerHeight - window.innerHeight;

  const l =
    Math.round(window.screenX * dpr) +
    Math.round(chromeSide * dpr) +
    Math.round(box.left * dpr);
  const t =
    Math.round(window.screenY * dpr) +
    Math.round(chromeTop * dpr) +
    Math.round(box.top * dpr);

  return {
    l,
    t,
    w: Math.max(0, Math.round(box.width * dpr)),
    h: Math.max(0, Math.round(box.height * dpr)),
  };
}

export function rectEquals(a: Rect, b: Rect): boolean {
  return a.l === b.l && a.t === b.t && a.w === b.w && a.h === b.h;
}
