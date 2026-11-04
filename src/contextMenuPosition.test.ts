import { describe, expect, it } from "vitest";
import { clampMenuPosition } from "./contextMenuPosition";

const base = { menuWidth: 200, menuHeight: 300, viewportWidth: 1000, viewportHeight: 800 };

describe("clampMenuPosition", () => {
  it("leaves a menu that already fits where the pointer is", () => {
    expect(clampMenuPosition({ ...base, x: 100, y: 50 })).toEqual({ left: 100, top: 50 });
  });

  it("shifts the menu up when it would run off the bottom", () => {
    const { top } = clampMenuPosition({ ...base, x: 100, y: 790 });
    expect(top + base.menuHeight).toBeLessThanOrEqual(base.viewportHeight);
    expect(top).toBeGreaterThanOrEqual(0);
  });

  it("shifts the menu left when it would run off the right edge", () => {
    const { left } = clampMenuPosition({ ...base, x: 990, y: 50 });
    expect(left + base.menuWidth).toBeLessThanOrEqual(base.viewportWidth);
    expect(left).toBeGreaterThanOrEqual(0);
  });

  it("pins a menu taller or wider than the viewport to the origin", () => {
    expect(
      clampMenuPosition({ ...base, x: 500, y: 500, menuHeight: 900, menuWidth: 1200 }),
    ).toEqual({ left: 0, top: 0 });
  });

  it("never returns negative coordinates", () => {
    expect(clampMenuPosition({ ...base, x: -20, y: -5 })).toEqual({ left: 0, top: 0 });
  });
});
