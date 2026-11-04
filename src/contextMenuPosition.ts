export type MenuPlacement = {
  /** Where the pointer was when the menu was requested. */
  x: number;
  y: number;
  menuWidth: number;
  menuHeight: number;
  viewportWidth: number;
  viewportHeight: number;
};

/**
 * Top-left corner for a context menu opened at (x, y) that keeps the menu
 * fully inside the viewport. A menu larger than the viewport is pinned to the
 * top-left edge (the caller is expected to cap its height and scroll it).
 */
export function clampMenuPosition(p: MenuPlacement): { left: number; top: number } {
  const clamp = (value: number, size: number, viewport: number): number =>
    Math.max(0, Math.min(value, viewport - size));
  return {
    left: clamp(p.x, p.menuWidth, p.viewportWidth),
    top: clamp(p.y, p.menuHeight, p.viewportHeight),
  };
}
