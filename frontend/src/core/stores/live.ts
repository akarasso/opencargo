// ---------------------------------------------------------------------------
// Live-resource helpers: bind a refetchable resource to WebSocket events.
// ---------------------------------------------------------------------------

import { createResource, onCleanup, type Resource } from 'solid-js';
import { onEvent } from '../ws.ts';

/**
 * Re-run `refetch` whenever one of `events` fires (plus on reconnect/resync),
 * debounced so a burst of publishes causes one refetch, not fifty.
 * Must be called inside a component/root so cleanup unsubscribes.
 */
export function useLive(
  refetch: () => unknown,
  events: string[],
  opts: { debounce?: number } = {},
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

  onCleanup(() => {
    if (timer) clearTimeout(timer);
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
  opts: { debounce?: number } = {},
): [Resource<T>, () => unknown] {
  const [data, { refetch }] = createResource(fetcher);
  useLive(refetch, events, opts);
  return [data, refetch];
}
