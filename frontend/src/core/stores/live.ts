// ---------------------------------------------------------------------------
// Live-resource helpers: bind a refetchable resource to WebSocket events.
// ---------------------------------------------------------------------------

import { createResource, onCleanup, type Resource } from 'solid-js';
import { onEvent } from '../ws.ts';

interface LiveOpts {
  debounce?: number;
  /**
   * Refetch every `pollMs` milliseconds — for data no WS event covers
   * (Prometheus metrics, health probes). Skipped while the tab is hidden.
   */
  pollMs?: number;
}

/**
 * Re-run `refetch` whenever one of `events` fires (plus on reconnect/resync),
 * debounced so a burst of publishes causes one refetch, not fifty.
 * Must be called inside a component/root so cleanup unsubscribes.
 */
export function useLive(
  refetch: () => unknown,
  events: string[],
  opts: LiveOpts = {},
): void {
  const wait = opts.debounce ?? 350;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const trigger = () => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      void refetch();
    }, wait);
  };

  const unsubs = [...events, '$connected', '$resync'].map((e) => onEvent(e, trigger));

  let poll: ReturnType<typeof setInterval> | null = null;
  if (opts.pollMs !== undefined) {
    poll = setInterval(() => {
      if (!document.hidden) void refetch();
    }, opts.pollMs);
  }

  onCleanup(() => {
    if (timer) clearTimeout(timer);
    if (poll) clearInterval(poll);
    unsubs.forEach((u) => u());
  });
}

/**
 * createResource + useLive in one call, for resources without a source signal.
 * Returns the resource and its refetch.
 */
export function createLiveResource<T>(
  fetcher: () => Promise<T>,
  events: string[],
  opts: LiveOpts = {},
): [Resource<T>, () => unknown] {
  const [data, { refetch }] = createResource(fetcher);
  useLive(refetch, events, opts);
  return [data, refetch];
}
