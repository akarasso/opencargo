// ---------------------------------------------------------------------------
// Live-resource helpers: bind a refetchable resource to WebSocket events.
// ---------------------------------------------------------------------------

import { createResource, onCleanup, type Resource } from 'solid-js';
import { onEvent } from '../ws.ts';

interface LiveOpts {
  debounce?: number;
  /**
   * Deadline armed by the first event of a burst: refetch by then even if
   * events keep re-arming the debounce (a stream faster than `debounce`
   * would otherwise never fire).
   */
  maxWait?: number;
  /**
   * Refetch every `pollMs` milliseconds — for data no WS event covers
   * (Prometheus metrics, health probes). Skipped while the tab is hidden.
   */
  pollMs?: number;
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
  return typeof value === 'object' && value !== null && 'then' in value;
}

/**
 * Re-run `refetch` whenever one of `events` fires (plus on reconnect/resync),
 * debounced so a burst of publishes causes one refetch, not fifty.
 * Must be called inside a component/root so cleanup unsubscribes.
 */
export function useLive(refetch: () => unknown, events: string[], opts: LiveOpts = {}): void {
  const wait = opts.debounce ?? 350;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let deadline: ReturnType<typeof setTimeout> | null = null;
  let inflight = false;
  let missed = false;

  const settle = () => {
    inflight = false;
    if (missed) trigger();
  };

  // A refetch still in flight absorbs the events that arrive meanwhile:
  // one more refetch follows it, never one per event.
  const fire = () => {
    if (timer) clearTimeout(timer);
    if (deadline) clearTimeout(deadline);
    timer = null;
    deadline = null;
    if (inflight) {
      missed = true;
      return;
    }
    missed = false;
    const result = refetch();
    if (isThenable(result)) {
      inflight = true;
      result.then(settle, settle);
    }
  };

  const trigger = () => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(fire, wait);
    if (opts.maxWait !== undefined && !deadline) deadline = setTimeout(fire, opts.maxWait);
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
    if (deadline) clearTimeout(deadline);
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
