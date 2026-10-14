import { describe, expect, it } from "vitest";
import { computeHidden, type VisibilityInputs } from "../src/index.js";

const V = (over: Partial<VisibilityInputs>): VisibilityInputs => ({
  concealed: false,
  domVisible: true,
  pageVisible: true,
  connected: true,
  ...over,
});

describe("computeHidden — overlay mirrors mount-element visibility", () => {
  it("is shown only when everything is visible", () => {
    expect(computeHidden(V({}))).toBe(false);
  });

  it("hides on any single invisible input", () => {
    expect(computeHidden(V({ concealed: true }))).toBe(true);
    expect(computeHidden(V({ domVisible: false }))).toBe(true);
    // Covered by another app / tab switched away / browser lost focus.
    expect(computeHidden(V({ pageVisible: false }))).toBe(true);
    // Element removed from the DOM.
    expect(computeHidden(V({ connected: false }))).toBe(true);
  });

  it("stays hidden when already concealed by the page", () => {
    expect(computeHidden(V({ concealed: true, pageVisible: false }))).toBe(true);
  });
});
