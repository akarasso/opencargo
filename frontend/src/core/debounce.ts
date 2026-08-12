// ---------------------------------------------------------------------------
// Debounce helper for Solid components.
// ---------------------------------------------------------------------------

import { onCleanup } from 'solid-js';

/**
 * Returns a debounced version of `fn`: each call restarts a `wait` ms timer,
 * and only the last call's arguments reach `fn`. Call it inside a component
 * (or any reactive owner) — the pending timer is cleared automatically on
 * disposal, so no timer outlives its component.
 */
export function createDebounced<Args extends unknown[]>(
  fn: (...args: Args) => void,
  wait: number,
): (...args: Args) => void {
  let timer: ReturnType<typeof setTimeout> | null = null;

  onCleanup(() => {
    if (timer) clearTimeout(timer);
  });

  return (...args: Args) => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      fn(...args);
    }, wait);
  };
}
