import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createRoot } from 'solid-js';
import { useLive } from './live.ts';

// live.ts pulls in ws.ts, which touches browser globals at module scope —
// replace it with an in-memory event bus (hoisted above the vi.mock factory).
const { bus } = vi.hoisted(() => {
  const listeners = new Map<string, Set<(event: { type: string }) => void>>();
  return {
    bus: {
      listeners,
      emit(type: string): void {
        listeners.get(type)?.forEach((h) => h({ type }));
      },
      reset(): void {
        listeners.clear();
      },
    },
  };
});

vi.mock('../ws.ts', () => ({
  onEvent: (type: string, handler: (event: { type: string }) => void): (() => void) => {
    let set = bus.listeners.get(type);
    if (!set) {
      set = new Set();
      bus.listeners.set(type, set);
    }
    set.add(handler);
    return () => set.delete(handler);
  },
}));

describe('useLive', () => {
  beforeEach(() => {
    bus.reset();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('debounces a burst of events into a single refetch (default 350ms)', () => {
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, ['package.published']);
      for (let i = 0; i < 50; i++) bus.emit('package.published');
      expect(refetch).not.toHaveBeenCalled();
      vi.advanceTimersByTime(349);
      expect(refetch).not.toHaveBeenCalled();
      vi.advanceTimersByTime(1);
      expect(refetch).toHaveBeenCalledTimes(1);
      dispose();
    });
  });

  it('restarts the debounce window on each new event', () => {
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, ['e']);
      bus.emit('e');
      vi.advanceTimersByTime(300);
      bus.emit('e'); // resets the 350ms window
      vi.advanceTimersByTime(300);
      expect(refetch).not.toHaveBeenCalled();
      vi.advanceTimersByTime(50);
      expect(refetch).toHaveBeenCalledTimes(1);
      dispose();
    });
  });

  it('honours a custom debounce value', () => {
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, ['e'], { debounce: 1000 });
      bus.emit('e');
      vi.advanceTimersByTime(999);
      expect(refetch).not.toHaveBeenCalled();
      vi.advanceTimersByTime(1);
      expect(refetch).toHaveBeenCalledTimes(1);
      dispose();
    });
  });

  it('also refetches on $connected and $resync', () => {
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, []);
      bus.emit('$resync');
      vi.advanceTimersByTime(350);
      expect(refetch).toHaveBeenCalledTimes(1);
      bus.emit('$connected');
      vi.advanceTimersByTime(350);
      expect(refetch).toHaveBeenCalledTimes(2);
      dispose();
    });
  });

  it('unsubscribes and cancels pending work on cleanup', () => {
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, ['e']);
      bus.emit('e');
      dispose(); // debounce is pending → must be cancelled
    });
    vi.advanceTimersByTime(1000);
    expect(refetch).not.toHaveBeenCalled();
    bus.emit('e'); // listener must be gone
    vi.advanceTimersByTime(1000);
    expect(refetch).not.toHaveBeenCalled();
  });

  it('polls while the tab is visible when pollMs is set', () => {
    vi.stubGlobal('document', { hidden: false });
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, [], { pollMs: 5000 });
      vi.advanceTimersByTime(5000);
      expect(refetch).toHaveBeenCalledTimes(1);
      vi.advanceTimersByTime(10_000);
      expect(refetch).toHaveBeenCalledTimes(3);
      dispose();
    });
    vi.advanceTimersByTime(20_000); // poll interval must stop with cleanup
    expect(refetch).toHaveBeenCalledTimes(3);
  });

  it('skips polling while the tab is hidden', () => {
    vi.stubGlobal('document', { hidden: true });
    const refetch = vi.fn();
    createRoot((dispose) => {
      useLive(refetch, [], { pollMs: 5000 });
      vi.advanceTimersByTime(30_000);
      expect(refetch).not.toHaveBeenCalled();
      dispose();
    });
  });
});
