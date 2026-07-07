// ---------------------------------------------------------------------------
// UI chrome state (sidebar drawer, command palette).
// ---------------------------------------------------------------------------

import { createRoot, createSignal } from 'solid-js';

function createUiStore() {
  const [sidebarOpen, setSidebarOpen] = createSignal(false);
  const [paletteOpen, setPaletteOpen] = createSignal(false);

  return {
    sidebarOpen,
    setSidebarOpen,
    toggleSidebar: () => setSidebarOpen((v) => !v),
    closeSidebar: () => setSidebarOpen(false),
    paletteOpen,
    setPaletteOpen,
    openPalette: () => setPaletteOpen(true),
    closePalette: () => setPaletteOpen(false),
  };
}

export const ui = createRoot(createUiStore);

/** Honor the user's reduced-motion preference in JS-driven animations. */
export const prefersReducedMotion = (): boolean =>
  window.matchMedia('(prefers-reduced-motion: reduce)').matches;
