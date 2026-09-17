// ---------------------------------------------------------------------------
// Policy report store: filters, the report resource and its live refetch.
// ---------------------------------------------------------------------------

import { batch, createMemo, createResource, createSignal } from 'solid-js';
import { fetchPolicyReport } from '../api.ts';
import type { PolicyEntry, PolicyQuery, PolicyReport, PolicyRules } from '../types.ts';
import { useLive } from './live.ts';

export const SINCE_OPTIONS = ['1h', '24h', '7d', '30d'] as const;
export const RULES = ['min_release_age', 'osv_severity', 'install_scripts', 'typosquat'] as const;
export const DEFAULT_SINCE = '24h';

export interface PolicyFilters {
  since: string;
  repo: string;
  rule: string;
  page: number;
}

/** Only set filters reach the query string; the server defaults cover the rest. */
export function toQuery(f: PolicyFilters): PolicyQuery {
  const q: PolicyQuery = { since: f.since };
  if (f.repo) q.repo = f.repo;
  if (f.rule) q.rule = f.rule;
  if (f.page > 1) q.page = f.page;
  return q;
}

export type Outcome = 'would_block' | 'unknown' | 'pass';

export function outcome(entry: Pick<PolicyEntry, 'would_block' | 'unknown'>): Outcome {
  if (entry.would_block) return 'would_block';
  if (entry.unknown) return 'unknown';
  return 'pass';
}

export function explain(entry: Pick<PolicyEntry, 'verdicts'>): string {
  const reasons = (kind: string) =>
    entry.verdicts.filter((v) => v.verdict === kind).map((v) => v.reason);
  const blocked = reasons('would_block');
  if (blocked.length > 0) return blocked.join('; ');
  const unknown = reasons('unknown');
  if (unknown.length > 0) return `unknown: ${unknown.join('; ')}`;
  return 'passed';
}

export function droppedBanner(report: Pick<PolicyReport, 'process'> | undefined): string | null {
  const dropped = report?.process?.dropped_since_start ?? 0;
  if (dropped <= 0) return null;
  return `${dropped.toLocaleString()} event${dropped === 1 ? '' : 's'} dropped since start`;
}

/** True when any filter left its default, so an empty page says "no match" not "nothing recorded". */
export function isFiltered(f: Pick<PolicyFilters, 'since' | 'repo' | 'rule'>): boolean {
  return f.since !== DEFAULT_SINCE || f.repo !== '' || f.rule !== '';
}

/**
 * Rules enabled on at least one proxy member, so the filter can label the others "off".
 * osv_severity needs the scanner too: with it off every row it produces is unknown.
 */
export function enabledRules(rules: PolicyRules | undefined): Set<string> {
  const on = new Set<string>();
  for (const cfg of Object.values(rules?.repositories ?? {})) {
    if (cfg.min_release_age) on.add('min_release_age');
    if (cfg.osv_severity && rules?.osv_enabled) on.add('osv_severity');
    if (cfg.install_scripts) on.add('install_scripts');
    if (cfg.typosquat) on.add('typosquat');
  }
  return on;
}

export function createPolicyStore() {
  const [since, setSinceRaw] = createSignal<string>(DEFAULT_SINCE);
  const [repo, setRepoRaw] = createSignal('');
  const [rule, setRuleRaw] = createSignal('');
  const [page, setPage] = createSignal(1);

  const query = createMemo(() =>
    toQuery({ since: since(), repo: repo(), rule: rule(), page: page() }),
  );
  const filtered = () => isFiltered({ since: since(), repo: repo(), rule: rule() });
  const [report, { refetch }] = createResource(query, (q) => fetchPolicyReport(q));
  useLive(refetch, ['policy.resolution'], { debounce: 300, maxWait: 2000 });

  const filter = (set: (v: string) => void) => (v: string) =>
    batch(() => {
      set(v);
      setPage(1);
    });

  return {
    since,
    repo,
    rule,
    page,
    query,
    filtered,
    report,
    refetch,
    setSince: filter(setSinceRaw),
    setRepo: filter(setRepoRaw),
    setRule: filter(setRuleRaw),
    setPage,
  };
}

export type PolicyStore = ReturnType<typeof createPolicyStore>;
