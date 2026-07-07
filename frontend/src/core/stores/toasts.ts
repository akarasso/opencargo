// ---------------------------------------------------------------------------
// Toast store — data only; rendering lives in components/Toaster.tsx.
// ---------------------------------------------------------------------------

import { createRoot, createSignal } from 'solid-js';

export type ToastKind = 'success' | 'error' | 'info' | 'event';

export interface Toast {
  id: number;
  kind: ToastKind;
  title: string;
  detail?: string;
  /** ms; 0 = sticky until dismissed */
  duration: number;
  /** set when the toast is animating out */
  leaving?: boolean;
}

function createToastStore() {
  const [toasts, setToasts] = createSignal<Toast[]>([]);
  let nextId = 1;

  function dismiss(id: number): void {
    // Two-phase removal so the exit animation can play.
    setToasts((list) => list.map((t) => (t.id === id ? { ...t, leaving: true } : t)));
    setTimeout(() => {
      setToasts((list) => list.filter((t) => t.id !== id));
    }, 220);
  }

  function push(kind: ToastKind, title: string, detail?: string, duration = 4200): number {
    const id = nextId++;
    setToasts((list) => [...list.slice(-4), { id, kind, title, detail, duration }]);
    if (duration > 0) setTimeout(() => dismiss(id), duration);
    return id;
  }

  return {
    toasts,
    dismiss,
    success: (title: string, detail?: string) => push('success', title, detail),
    error: (title: string, detail?: string) => push('error', title, detail, 6500),
    info: (title: string, detail?: string) => push('info', title, detail),
    /** Real-time happenings (package published…), slightly shorter-lived. */
    event: (title: string, detail?: string) => push('event', title, detail, 3600),
  };
}

export const toasts = createRoot(createToastStore);
