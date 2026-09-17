import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createRoot } from 'solid-js';
import type { PolicyEntry, PolicyReport, PolicyRules, PolicyVerdict } from '../types.ts';
import {
  createPolicyStore,
  droppedBanner,
  enabledRules,
  explain,
  isFiltered,
  outcome,
  REFETCH_DEBOUNCE,
  REFETCH_MAX_WAIT,
  toQuery,
} from './policy.ts';

const { bus, api } = vi.hoisted(() => {
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
    api: { fetchPolicyReport: vi.fn() },
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

vi.mock('../api.ts', () => api);

const report: PolicyReport = {
  since: '2026-09-16T10:00:00Z',
  page: 1,
  size: 50,
  totals: { resolutions: 0, would_block: 0, unknown: 0, by_rule: {} },
  entries: [],
};

function verdicts(
  ...vs: [string, PolicyVerdict['verdict'], string][]
): Pick<PolicyEntry, 'verdicts'> {
  return { verdicts: vs.map(([rule, verdict, reason]) => ({ rule, verdict, reason })) };
}

describe('toQuery', () => {
  it('serialises only set filters', () => {
    expect(toQuery({ since: '24h', repo: '', rule: '', page: 1 })).toEqual({ since: '24h' });
    expect(toQuery({ since: '7d', repo: 'npm-all', rule: 'typosquat', page: 3 })).toEqual({
      since: '7d',
      repo: 'npm-all',
      rule: 'typosquat',
      page: 3,
    });
  });
});

describe('createPolicyStore', () => {
  beforeEach(() => {
    bus.reset();
    vi.useFakeTimers();
    api.fetchPolicyReport.mockReset();
    api.fetchPolicyReport.mockResolvedValue(report);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('changing repo resets page to 1 and refetches once with the new query', async () => {
    let store!: ReturnType<typeof createPolicyStore>;
    const dispose = createRoot((d) => {
      store = createPolicyStore();
      return d;
    });
    await Promise.resolve();
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(1);
    expect(api.fetchPolicyReport).toHaveBeenLastCalledWith({ since: '24h' });
    store.setPage(3);
    await Promise.resolve();
    expect(api.fetchPolicyReport).toHaveBeenLastCalledWith({ since: '24h', page: 3 });
    expect(store.filtered()).toBe(false);
    store.setRepo('npm-all');
    await Promise.resolve();
    expect(store.page()).toBe(1);
    expect(store.filtered()).toBe(true);
    expect(store.query()).toEqual({ since: '24h', repo: 'npm-all' });
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(3);
    expect(api.fetchPolicyReport).toHaveBeenLastCalledWith({ since: '24h', repo: 'npm-all' });
    dispose();
  });

  it('a policy.resolution event triggers one debounced refetch', async () => {
    const dispose = createRoot((d) => {
      createPolicyStore();
      return d;
    });
    await Promise.resolve();
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(1);
    for (let i = 0; i < 20; i++) bus.emit('policy.resolution');
    await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE - 1);
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(5000);
    expect(api.fetchPolicyReport).toHaveBeenCalledTimes(2);
    dispose();
  });

  it('refetches at a floor rate under a stream of flushes', async () => {
    const dispose = createRoot((d) => {
      createPolicyStore();
      return d;
    });
    await Promise.resolve();
    expect(REFETCH_DEBOUNCE).toBeGreaterThan(500);
    for (let t = 0; t < 10_000; t += 100) {
      bus.emit('policy.resolution');
      await vi.advanceTimersByTimeAsync(100);
    }
    const streamed = api.fetchPolicyReport.mock.calls.length - 1;
    expect(streamed).toBeGreaterThanOrEqual(3);
    expect(streamed).toBeLessThanOrEqual(10_000 / REFETCH_MAX_WAIT + 1);
    await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE);
    expect(api.fetchPolicyReport.mock.calls.length - 1).toBe(streamed + 1);
    dispose();
  });
});

describe('explain', () => {
  it('prefers would_block, then unknown:, then passed', () => {
    expect(
      explain(
        verdicts(
          ['osv_severity', 'pass', 'no advisory'],
          ['min_release_age', 'would_block', 'published 2h ago, threshold 24h'],
          ['typosquat', 'unknown', 'not resolved'],
          ['install_scripts', 'would_block', 'declares postinstall'],
        ),
      ),
    ).toBe('published 2h ago, threshold 24h; declares postinstall');
    expect(
      explain(
        verdicts(
          ['osv_severity', 'pass', 'no advisory'],
          ['min_release_age', 'unknown', 'no publish date (timeout)'],
        ),
      ),
    ).toBe('unknown: no publish date (timeout)');
    expect(explain(verdicts(['osv_severity', 'pass', 'no advisory']))).toBe('passed');
    expect(explain(verdicts(['typosquat', 'not_applicable', 'oci']))).toBe('passed');
    expect(explain(verdicts())).toBe('passed');
  });
});

describe('outcome', () => {
  it('follows the denormalised flags, would_block first', () => {
    expect(outcome({ would_block: true, unknown: true })).toBe('would_block');
    expect(outcome({ would_block: false, unknown: true })).toBe('unknown');
    expect(outcome({ would_block: false, unknown: false })).toBe('pass');
  });
});

describe('droppedBanner', () => {
  it('renders only when dropped_since_start > 0', () => {
    expect(droppedBanner(undefined)).toBeNull();
    expect(droppedBanner({})).toBeNull();
    expect(droppedBanner({ process: { dropped_since_start: 0 } })).toBeNull();
    expect(droppedBanner({ process: { dropped_since_start: 1 } })).toBe(
      '1 event dropped since start',
    );
    expect(droppedBanner({ process: { dropped_since_start: 41 } })).toBe(
      '41 events dropped since start',
    );
  });
});

describe('isFiltered', () => {
  it('is true once any of since, repo or rule leaves its default', () => {
    expect(isFiltered({ since: '24h', repo: '', rule: '' })).toBe(false);
    expect(isFiltered({ since: '1h', repo: '', rule: '' })).toBe(true);
    expect(isFiltered({ since: '24h', repo: 'npm-all', rule: '' })).toBe(true);
    expect(isFiltered({ since: '24h', repo: '', rule: 'typosquat' })).toBe(true);
  });
});

describe('enabledRules', () => {
  it('collects rules on for at least one member, osv_severity only with the scanner on', () => {
    const rules: PolicyRules = {
      osv_enabled: false,
      recording: ['npm-proxy'],
      repositories: {
        'npm-proxy': {
          min_release_age: '48h',
          osv_severity: 'high',
          install_scripts: false,
          typosquat: true,
          fetch_missing_facts: true,
        },
        'cargo-proxy': {
          min_release_age: null,
          osv_severity: null,
          install_scripts: false,
          typosquat: false,
          fetch_missing_facts: true,
        },
      },
    };
    expect([...enabledRules(rules)].sort()).toEqual(['min_release_age', 'typosquat']);
    expect([...enabledRules({ ...rules, osv_enabled: true })].sort()).toEqual([
      'min_release_age',
      'osv_severity',
      'typosquat',
    ]);
    expect(enabledRules(undefined).size).toBe(0);
  });
});
