import { For, Show, createResource } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { FormatTag, LoadError, StatsSkeleton, TableSkeleton } from '../../components/bits.tsx';
import { fetchPolicyRules, fetchRepositories } from '../../core/api.ts';
import {
  RULES,
  SINCE_OPTIONS,
  createPolicyStore,
  droppedBanner,
  enabledRules,
  explain,
  outcome,
  type PolicyStore,
} from '../../core/stores/policy.ts';
import { wsStatus } from '../../core/ws.ts';
import { timeAgo } from '../../core/format.ts';
import type { PolicyEntry, PolicyRules, PolicyTotals } from '../../core/types.ts';

const CHIP = {
  would_block: ['chip chip-danger', 'would block'],
  unknown: ['chip chip-warn', 'unknown'],
  pass: ['chip chip-ok', 'pass'],
} as const;

export default function PolicyReport() {
  return (
    <RequireAdmin>
      <PolicyReportInner />
    </RequireAdmin>
  );
}

function PolicyReportInner() {
  const store = createPolicyStore();
  const [rules] = createResource(fetchPolicyRules);
  const [repos] = createResource(fetchRepositories);
  const repoNames = () =>
    (repos()?.repositories ?? []).filter((r) => r.type !== 'hosted').map((r) => r.name);

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Policy report</h1>
          <p class="page-sub">
            What each policy rule would have blocked. Nothing is blocked: every download listed here
            was served.
          </p>
        </div>
        <span class={`feed-live ${wsStatus() === 'online' ? 'online' : ''}`}>
          <span class="conn-dot" />
          {wsStatus() === 'online' ? 'streaming' : 'paused'}
        </span>
      </div>

      <FilterBar store={store} repos={repoNames()} rules={rules()} />

      <Show when={store.report.error}>
        <LoadError what="the policy report" />
      </Show>

      <Show
        when={store.report()}
        fallback={
          <>
            <StatsSkeleton />
            <TableSkeleton rows={8} cols={6} />
          </>
        }
      >
        {(d) => (
          <>
            <Show when={droppedBanner(d())}>
              {(text) => (
                <div class="alert alert-warn" role="alert">
                  <Icon name="alert-triangle" size={16} />
                  <div>
                    <div style={{ 'font-weight': 600 }}>{text()}</div>
                    <div class="small" style={{ opacity: 0.85 }}>
                      The recorder queue overflowed; those downloads were served but never examined.
                    </div>
                  </div>
                </div>
              )}
            </Show>
            <Tiles totals={d().totals} />
            <Show
              when={d().entries.length > 0}
              fallback={<EmptyReport rules={rules()} filtered={store.filtered()} />}
            >
              <ReportTable entries={d().entries} page={d().page} size={d().size} store={store} />
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

function FilterBar(props: { store: PolicyStore; repos: string[]; rules: PolicyRules | undefined }) {
  const on = () => enabledRules(props.rules);
  return (
    <div class="filter-bar">
      <select
        class="select"
        value={props.store.since()}
        onChange={(e) => props.store.setSince(e.currentTarget.value)}
      >
        <For each={[...SINCE_OPTIONS]}>{(s) => <option value={s}>Last {s}</option>}</For>
      </select>
      <select
        class="select"
        value={props.store.repo()}
        onChange={(e) => props.store.setRepo(e.currentTarget.value)}
      >
        <option value="">All repositories</option>
        <For each={props.repos}>{(name) => <option value={name}>{name}</option>}</For>
      </select>
      <select
        class="select"
        value={props.store.rule()}
        onChange={(e) => props.store.setRule(e.currentTarget.value)}
      >
        <option value="">All rules</option>
        <For each={[...RULES]}>
          {(rule) => (
            <option value={rule}>
              {rule}
              {props.rules && !on().has(rule) ? ' (off)' : ''}
            </option>
          )}
        </For>
      </select>
    </div>
  );
}

function Tiles(props: { totals: PolicyTotals }) {
  return (
    <section class="stats-grid cols-3 section">
      <div class="stat" style={{ '--stat-tint': 'var(--accent)' }}>
        <div class="stat-head">
          <span class="stat-label">Resolutions</span>
          <Icon name="download" size={16} />
        </div>
        <div class="stat-value">{props.totals.resolutions.toLocaleString()}</div>
        <div class="stat-foot">artifacts served through recording members</div>
      </div>
      <div class="stat" style={{ '--stat-tint': 'var(--danger)' }}>
        <div class="stat-head">
          <span class="stat-label">Would block</span>
          <Icon name="shield" size={16} />
        </div>
        <div class="stat-value">{props.totals.would_block.toLocaleString()}</div>
        <div class="stat-foot">at least one rule would have refused</div>
      </div>
      <div class="stat" style={{ '--stat-tint': 'var(--warn)' }}>
        <div class="stat-head">
          <span class="stat-label">Unknown</span>
          <Icon name="alert-circle" size={16} />
        </div>
        <div class="stat-value">{props.totals.unknown.toLocaleString()}</div>
        <div class="stat-foot">a rule lacked a fact to decide</div>
      </div>
    </section>
  );
}

function EmptyReport(props: { rules: PolicyRules | undefined; filtered: boolean }) {
  const recording = () => props.rules?.recording ?? [];
  const text = () => {
    if (props.filtered) return 'No resolution matches these filters.';
    if (recording().length === 0)
      return 'No proxy member records: enable a rule under [policy.<repo>] in the config.';
    return `Recording members: ${recording().join(', ')}. Rows appear as artifacts are served.`;
  };
  return (
    <div class="card">
      <EmptyState icon="shield" title="Nothing recorded yet" text={text()} />
    </div>
  );
}

function VerdictChip(props: { entry: PolicyEntry }) {
  const chip = () => CHIP[outcome(props.entry)];
  return <span class={chip()[0]}>{chip()[1]}</span>;
}

function ReportTable(props: {
  entries: PolicyEntry[];
  page: number;
  size: number;
  store: PolicyStore;
}) {
  return (
    <div class="table-card page-enter">
      <div class="table-scroll">
        <table class="table">
          <thead>
            <tr>
              <th>When</th>
              <th>Actor</th>
              <th>Artifact</th>
              <th class="cell-hide-sm">Repo</th>
              <th>Verdict</th>
              <th>Why</th>
            </tr>
          </thead>
          <tbody>
            <For each={props.entries}>{(entry) => <Row entry={entry} />}</For>
          </tbody>
        </table>
      </div>
      <div class="pagination">
        <span class="pagination-info">Page {props.page}</span>
        <div class="pagination-nav">
          <button
            class="btn btn-ghost btn-sm"
            disabled={props.store.page() <= 1}
            onClick={() => props.store.setPage((p) => p - 1)}
          >
            <Icon name="chevron-left" size={14} />
            Newer
          </button>
          <button
            class="btn btn-ghost btn-sm"
            disabled={props.entries.length < props.size}
            onClick={() => props.store.setPage((p) => p + 1)}
          >
            Older
            <Icon name="chevron-right" size={14} />
          </button>
        </div>
      </div>
    </div>
  );
}

function Row(props: { entry: PolicyEntry }) {
  const artifact = () =>
    props.entry.version ? `${props.entry.name}@${props.entry.version}` : props.entry.name;
  const route = () =>
    props.entry.requested_repo === props.entry.member_repo
      ? props.entry.member_repo
      : `${props.entry.requested_repo} → ${props.entry.member_repo}`;
  return (
    <tr>
      <td class="cell-dim nowrap" title={props.entry.created_at}>
        {timeAgo(props.entry.created_at)}
      </td>
      <td>
        <span class="mono small" style={{ color: 'var(--ink)' }}>
          {props.entry.actor}
        </span>
        <span class="dim small"> {props.entry.actor_kind}</span>
      </td>
      <td>
        <div class="row">
          <span class="mono small truncate" style={{ 'max-width': '260px' }} title={artifact()}>
            {artifact()}
          </span>
          <FormatTag format={props.entry.format} />
        </div>
      </td>
      <td class="cell-mono cell-dim cell-hide-sm">{route()}</td>
      <td>
        <VerdictChip entry={props.entry} />
      </td>
      <td class="cell-muted truncate" style={{ 'max-width': '360px' }} title={explain(props.entry)}>
        {explain(props.entry)}
      </td>
    </tr>
  );
}
