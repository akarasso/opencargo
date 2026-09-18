import { For, Show, createResource, createSignal } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import {
  addMcpRule,
  decideMcp,
  deleteMcpRule,
  fetchMcpEvidence,
  fetchMcpRules,
  fetchMcpServers,
  fetchRepositories,
  probeMcp,
  suppressMcpFinding,
  syncMcp,
} from '../../core/api.ts';
import { useLive } from '../../core/stores/live.ts';
import { reportError, toasts } from '../../core/stores/toasts.ts';
import { timeAgo } from '../../core/format.ts';
import type { McpEvidence, McpServerRow, McpState } from '../../core/types.ts';

const STATES = ['all', 'pending', 'drifted', 'blocked', 'approved', 'not_observed'] as const;

const CHIP: Record<McpState, string> = {
  approved: 'chip chip-ok',
  pending: 'chip chip-warn',
  drifted: 'chip chip-danger',
  blocked: 'chip chip-danger',
};

export default function McpGovernance() {
  return (
    <RequireAdmin>
      <McpGovernanceInner />
    </RequireAdmin>
  );
}

function transports(row: McpServerRow): string {
  const parts = [];
  if (row.transports.packages) parts.push(`pkg: ${row.transports.packages}`);
  if (row.transports.remotes) parts.push(`remote: ${row.transports.remotes}`);
  return parts.join(' / ') || '—';
}

function McpGovernanceInner() {
  const [repos] = createResource(fetchRepositories);
  const mcpRepos = () =>
    (repos()?.repositories ?? []).filter((r) => r.format === 'mcp').map((r) => r.name);
  const [repo, setRepo] = createSignal('');
  const [state, setState] = createSignal<string>('all');
  const [q, setQ] = createSignal('');
  const [open, setOpen] = createSignal<McpServerRow | null>(null);
  const current = () => repo() || mcpRepos()[0] || '';

  const [servers, { refetch }] = createResource(
    () => (current() ? { repo: current(), state: state(), q: q() } : null),
    (k) => fetchMcpServers(k.repo, k.state, k.q),
  );
  const [rules, rulesCtl] = createResource(
    () => current() || null,
    (r) => fetchMcpRules(r),
  );
  useLive(refetch, ['mcp.sync', 'mcp.drift'], { debounce: 300, maxWait: 2000 });

  const act = async (title: string, run: () => Promise<unknown>) => {
    try {
      await run();
      toasts.success(title);
      await refetch();
    } catch (e) {
      reportError(`${title} failed`, e);
    }
  };

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">MCP servers and skills</h1>
          <p class="page-sub">
            What each repository lets agents install: approvals bound to fingerprinted surfaces,
            allow rules, and the evidence behind every finding.
          </p>
        </div>
        <div class="row">
          <button
            class="btn btn-ghost btn-sm"
            onClick={() => act('Sync', () => syncMcp(current()))}
          >
            <Icon name="refresh" size={14} /> Sync
          </button>
          <button
            class="btn btn-ghost btn-sm"
            onClick={() => act('Probe', () => probeMcp(current()))}
          >
            <Icon name="activity" size={14} /> Probe
          </button>
        </div>
      </div>

      <div class="filter-bar">
        <select class="select" value={current()} onChange={(e) => setRepo(e.currentTarget.value)}>
          <For each={mcpRepos()}>{(name) => <option value={name}>{name}</option>}</For>
        </select>
        <For each={[...STATES]}>
          {(s) => (
            <button
              class={`chip ${state() === s ? 'chip-accent' : ''}`}
              onClick={() => setState(s)}
            >
              {s.replace('_', ' ')}
            </button>
          )}
        </For>
        <input
          class="input"
          placeholder="Filter by name"
          value={q()}
          onInput={(e) => setQ(e.currentTarget.value)}
        />
      </div>

      <Show when={servers()?.sync?.lastError}>
        {(err) => (
          <div class="alert alert-warn" role="alert">
            <Icon name="alert-triangle" size={16} />
            <div>Last sync failed: {err()}. The mirror keeps serving its last good rows.</div>
          </div>
        )}
      </Show>

      <Show when={servers.error}>
        <LoadError what="the MCP catalog" />
      </Show>
      <Show when={!current()}>
        <div class="card">
          <EmptyState
            icon="database"
            title="No MCP repository"
            text="Create a repository with the mcp format first."
          />
        </div>
      </Show>

      <Show
        when={servers()}
        fallback={
          <Show when={current()}>
            <TableSkeleton rows={6} cols={6} />
          </Show>
        }
      >
        {(d) => (
          <div class="table-card">
            <div class="table-scroll">
              <table class="table">
                <thead>
                  <tr>
                    <th>Server</th>
                    <th>State</th>
                    <th class="cell-hide-sm">Transports</th>
                    <th>Tools</th>
                    <th>Findings</th>
                    <th class="cell-hide-sm">Synced</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={d().servers}>
                    {(row) => (
                      <tr class="clickable" onClick={() => setOpen(row)}>
                        <td class="cell-mono">
                          {row.name}@{row.version}
                        </td>
                        <td>
                          <span class={CHIP[row.state]}>{row.state}</span>
                          <Show when={row.drift === 'new_endpoint'}>
                            <span class="dim small"> a new endpoint appeared</span>
                          </Show>
                        </td>
                        <td class="cell-dim small cell-hide-sm">{transports(row)}</td>
                        <td>
                          <span class={row.toolsSource === 'declared' ? 'chip' : 'chip chip-ok'}>
                            {row.toolsSource === 'declared'
                              ? 'tools: not observed'
                              : row.toolsSource}
                          </span>
                        </td>
                        <td>
                          <span
                            class={
                              row.findings.high > 0
                                ? 'chip chip-danger'
                                : row.findings.medium > 0
                                  ? 'chip chip-warn'
                                  : 'chip'
                            }
                          >
                            {row.findings.high} high · {row.findings.medium} medium
                          </span>
                        </td>
                        <td class="cell-dim nowrap cell-hide-sm">{timeAgo(row.syncedAt)}</td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          </div>
        )}
      </Show>

      <Show when={open()}>
        {(row) => <Detail repo={current()} row={row()} onClose={() => setOpen(null)} act={act} />}
      </Show>

      <Show when={current()}>
        <div class="card section">
          <div class="section-title">Allow rules</div>
          <p class="dim small">
            An exact name, or a prefix ending in <span class="mono">/*</span> or{' '}
            <span class="mono">.*</span>. The first allow rule closes the repository to everything
            it does not match.
          </p>
          <ul class="list">
            <For each={rules() ?? []}>
              {(rule) => (
                <li class="row">
                  <span class={rule.effect === 'allow' ? 'chip chip-ok' : 'chip chip-danger'}>
                    {rule.effect}
                  </span>
                  <span class="mono">{rule.pattern}</span>
                  <button
                    class="btn btn-ghost btn-sm"
                    onClick={() =>
                      act('Rule removed', async () => {
                        await deleteMcpRule(current(), rule.id);
                        await rulesCtl.refetch();
                      })
                    }
                  >
                    <Icon name="trash" size={14} />
                  </button>
                </li>
              )}
            </For>
          </ul>
          <RuleForm
            onAdd={(pattern, effect) =>
              act('Rule added', async () => {
                await addMcpRule(current(), pattern, effect);
                await rulesCtl.refetch();
              })
            }
          />
        </div>
      </Show>
    </div>
  );
}

function RuleForm(props: { onAdd: (pattern: string, effect: 'allow' | 'deny') => void }) {
  const [pattern, setPattern] = createSignal('');
  const [effect, setEffect] = createSignal<'allow' | 'deny'>('allow');
  return (
    <div class="row">
      <input
        class="input"
        placeholder="io.github.acme/*"
        value={pattern()}
        onInput={(e) => setPattern(e.currentTarget.value)}
      />
      <select
        class="select"
        value={effect()}
        onChange={(e) => setEffect(e.currentTarget.value as 'allow' | 'deny')}
      >
        <option value="allow">allow</option>
        <option value="deny">deny</option>
      </select>
      <button
        class="btn btn-primary btn-sm"
        disabled={!pattern()}
        onClick={() => props.onAdd(pattern(), effect())}
      >
        Add
      </button>
    </div>
  );
}

function Detail(props: {
  repo: string;
  row: McpServerRow;
  onClose: () => void;
  act: (title: string, run: () => Promise<unknown>) => Promise<void>;
}) {
  const [evidence, { refetch }] = createResource(
    () => ({ repo: props.repo, name: props.row.name, version: props.row.version }),
    (k) => fetchMcpEvidence(k.repo, k.name, k.version),
  );
  const decide = (state: 'approved' | 'blocked') =>
    props.act(state === 'approved' ? 'Approved' : 'Blocked', async () => {
      await decideMcp(props.repo, { name: props.row.name, version: props.row.version, state });
      await refetch();
    });
  return (
    <div class="card section">
      <div class="row" style={{ 'justify-content': 'space-between' }}>
        <div class="section-title mono">
          {props.row.name}@{props.row.version}
        </div>
        <div class="row">
          <button class="btn btn-primary btn-sm" onClick={() => decide('approved')}>
            Approve
          </button>
          <button class="btn btn-danger btn-sm" onClick={() => decide('blocked')}>
            Block
          </button>
          <button
            class="btn btn-ghost btn-sm"
            onClick={() =>
              props.act('Probed', async () => {
                await probeMcp(props.repo, props.row.name, props.row.version);
                await refetch();
              })
            }
          >
            Re-probe
          </button>
          <button class="btn btn-ghost btn-sm" onClick={() => props.onClose()}>
            <Icon name="x" size={14} />
          </button>
        </div>
      </div>
      <Show when={evidence()} fallback={<TableSkeleton rows={3} cols={3} />}>
        {(e) => <Evidence e={e()} repo={props.repo} act={props.act} refetch={refetch} />}
      </Show>
    </div>
  );
}

function Evidence(props: {
  e: McpEvidence;
  repo: string;
  act: (title: string, run: () => Promise<unknown>) => Promise<void>;
  refetch: () => unknown;
}) {
  return (
    <>
      <div class="dim small">
        {props.e.row.endpoints.approved} of {props.e.row.endpoints.total} endpoints approved · drift{' '}
        {props.e.row.drift}
        <Show when={props.e.row.driftedRemote}> at {props.e.row.driftedRemote}</Show>
      </div>
      <For each={props.e.surfaces}>
        {(s) => (
          <div class="section">
            <div class="small">
              <span class="chip">{s.source}</span>{' '}
              <span class="mono">{s.remoteUrl || 'declared record'}</span>{' '}
              <span class="dim">captured {timeAgo(s.capturedAt)}</span>
            </div>
            <ul class="list small">
              <For each={s.tools}>
                {(t) => (
                  <li>
                    <span class="mono">{t.name}</span> <span class="dim">{t.description}</span>
                  </li>
                )}
              </For>
              <For each={s.headerParams}>
                {(h) => (
                  <li>
                    <span class={h.valid ? 'chip chip-warn' : 'chip chip-danger'}>
                      {h.valid ? h.header : `invalid: ${h.why}`}
                    </span>{' '}
                    <span class="mono">
                      {h.tool} {h.pointer}
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </div>
        )}
      </For>
      <div class="section-title">Findings</div>
      <Show when={props.e.findings.length > 0} fallback={<div class="dim small">No finding.</div>}>
        <ul class="list">
          <For each={props.e.findings}>
            {(f) => (
              <li class="small">
                <span class={f.confidence === 'high' ? 'chip chip-danger' : 'chip chip-warn'}>
                  {f.pattern}
                  {f.promotedBy ? ` (via ${f.promotedBy})` : ''}
                </span>{' '}
                <span class="mono dim">
                  {f.field}
                  {f.tool ? ` / ${f.tool}` : ''}
                </span>
                <pre class="mono small">{f.excerpt}</pre>
                <Show when={!f.suppressed} fallback={<span class="dim">suppressed here</span>}>
                  <button
                    class="btn btn-ghost btn-sm"
                    onClick={() =>
                      props.act('Suppressed', async () => {
                        await suppressMcpFinding(props.repo, f.pattern, f.tool);
                        await props.refetch();
                      })
                    }
                  >
                    Suppress this pattern on this tool
                  </button>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
      <div class="section-title">Probe history</div>
      <ul class="list small">
        <For each={props.e.probeRuns}>
          {(r) => (
            <li>
              <span class={r.ok ? 'chip chip-ok' : 'chip chip-danger'}>
                {r.ok ? 'ok' : 'failed'}
              </span>{' '}
              <span class="mono">{r.remoteUrl}</span> <span class="dim">{timeAgo(r.ranAt)}</span>{' '}
              {r.protocolVersion ?? ''} {r.error ?? ''}
            </li>
          )}
        </For>
      </ul>
    </>
  );
}
