// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { computeScreenRect, rectEquals } from "../src/rect";

function stubWindow(opts: {
  screenX: number;
  outerWidth: number;
  innerWidth: number;
  outerHeight: number;
  innerHeight: number;
  dpr: number;
}): void {
  Object.defineProperty(window, "screenX", { value: opts.screenX, configurable: true });
  Object.defineProperty(window, "outerWidth", { value: opts.outerWidth, configurable: true });
  Object.defineProperty(window, "innerWidth", { value: opts.innerWidth, configurable: true });
  Object.defineProperty(window, "outerHeight", { value: opts.outerHeight, configurable: true });
  Object.defineProperty(window, "innerHeight", { value: opts.innerHeight, configurable: true });
  Object.defineProperty(window, "devicePixelRatio", { value: opts.dpr, configurable: true });
}

function stubElementRect(left: number, top: number, width: number, height: number): void {
  const el = document.createElement("div");
  vi.spyOn(el, "getBoundingClientRect").mockReturnValue({
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    x: left,
    y: top,
    toJSON: () => ({}),
  } as DOMRect);
  document.body.appendChild(el);
  // The function under test accepts any HTMLElement; return this one.
  (globalThis as { __testEl?: HTMLElement }).__testEl = el;
}

describe("computeScreenRect", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("computes physical-pixel screen rect incl. chrome offsets and DPR", () => {
    stubWindow({ screenX: 100, outerWidth: 1220, innerWidth: 1200, outerHeight: 1100, innerHeight: 1000, dpr: 2 });
    stubElementRect(50, 25, 400, 300);

    const el = (globalThis as { __testEl?: HTMLElement }).__testEl!;
    const r = computeScreenRect(el);
    // chromeSide = (1220-1200)/2 = 10 CSS px; chromeTop = 1100-1000 = 100 CSS px
    // l = 100*2 + 10*2 + 50*2 = 320; t = screenY(0)*2 + 100*2 + 25*2 = 250
    expect(r).toEqual({ l: 320, t: 250, w: 800, h: 600 });
  });

  it("fullscreen (F11) has no chrome offset", () => {
    stubWindow({ screenX: 0, outerWidth: 1920, innerWidth: 1920, outerHeight: 1080, innerHeight: 1080, dpr: 1 });
    stubElementRect(0, 0, 1920, 1080);
    const el = (globalThis as { __testEl?: HTMLElement }).__testEl!;
    expect(computeScreenRect(el)).toEqual({ l: 0, t: 0, w: 1920, h: 1080 });
  });

  it("browser zoom (dpr) scales the rect", () => {
    stubWindow({ screenX: 0, outerWidth: 1000, innerWidth: 1000, outerHeight: 800, innerHeight: 700, dpr: 1.5 });
    stubElementRect(10, 10, 200, 100);
    const el = (globalThis as { __testEl?: HTMLElement }).__testEl!;
    const r = computeScreenRect(el);
    expect(r.w).toBe(300);
    expect(r.h).toBe(150);
    expect(r.l).toBe(15);
    expect(r.t).toBe(Math.round(0 * 1.5) + Math.round(100 * 1.5) + Math.round(10 * 1.5)); // 165
  });

  it("rect equality helper", () => {
    expect(rectEquals({ l: 1, t: 2, w: 3, h: 4 }, { l: 1, t: 2, w: 3, h: 4 })).toBe(true);
    expect(rectEquals({ l: 1, t: 2, w: 3, h: 4 }, { l: 9, t: 2, w: 3, h: 4 })).toBe(false);
  });
});
