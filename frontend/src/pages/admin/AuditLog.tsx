import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import { fetchAudit } from '../../core/api.ts';
import { useLive } from '../../core/stores/live.ts';
import { onEvent, wsStatus } from '../../core/ws.ts';
import { initials, timeAgo } from '../../core/format.ts';

function actionChip(action: string): string {
  if (action.includes('delete') || action.includes('revoke') || action.includes('remove'))
    return 'chip chip-danger';
  if (action.includes('create') || action.includes('publish')) return 'chip chip-ok';
  if (action.includes('permission') || action.includes('password')) return 'chip chip-warn';
  if (action.includes('promote')) return 'chip chip-accent';
  return 'chip chip-neutral';
}

export default function AuditLog() {
  return (
    <RequireAdmin>
      <AuditLogInner />
    </RequireAdmin>
  );
}

interface LiveEntry {
  key: string;
  username: string;
  action: string;
  target: string | null;
  ts: string;
}

function AuditLogInner() {
  const [page, setPage] = createSignal(1);
  const pageSize = 50;

  const [data, { refetch }] = createResource(page, (p) => fetchAudit(p, pageSize));
  useLive(refetch, [], { debounce: 800 }); // reconnect/resync only — live rows come via WS below

  // Entries arriving over the WebSocket while we watch (page 1 view).
  const [liveEntries, setLiveEntries] = createSignal<LiveEntry[]>([]);
  onMount(() => {
    const unsub = onEvent('audit.entry', (ev) => {
      const d = ev.data ?? {};
      setLiveEntries((rows) =>
        [
          {
            key: `${ev.ts}-${d.action}-${d.target ?? ''}`,
            username: String(d.username ?? '—'),
            action: String(d.action ?? 'unknown'),
            target: d.target == null ? null : String(d.target),
            ts: ev.ts ?? '',
          },
          ...rows,
        ].slice(0, 30),
      );
    });
    onCleanup(unsub);
  });

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Audit log</h1>
          <p class="page-sub">
            Every sensitive action — publishes, permission changes, user and repository
            administration — with who did it and when.
          </p>
        </div>
        <span class={`feed-live ${wsStatus() === 'online' ? 'online' : ''}`}>
          <span class="conn-dot" />
          {wsStatus() === 'online' ? 'streaming' : 'paused'}
        </span>
      </div>

      <Show when={liveEntries().length > 0}>
        <div class="feed-card section">
          <div class="feed-head">
            <span class="section-title">Just happened</span>
            <span class="dim small">{liveEntries().length} since you opened this page</span>
          </div>
          <ul class="feed">
            <For each={liveEntries()}>
              {(e) => (
                <li class="feed-row fresh">
                  <span class="feed-time" title={e.ts}>
                    {timeAgo(e.ts)}
                  </span>
                  <div class="feed-main">
                    <span class="mono small" style={{ color: 'var(--ink)' }}>
                      {e.username}
                    </span>
                    <span class={actionChip(e.action)}>{e.action}</span>
                    <Show when={e.target}>
                      <span class="dim small mono truncate">{e.target}</span>
                    </Show>
                  </div>
                </li>
              )}
            </For>
          </ul>
        </div>
      </Show>

      <Show when={data.error}>
        <LoadError what="the audit log" />
      </Show>

      <Show when={data()} fallback={<TableSkeleton rows={8} cols={5} />}>
        {(d) => (
          <Show
            when={d().entries.length > 0}
            fallback={
              <div class="card">
                <EmptyState icon="history" title="Nothing recorded yet" text="Sensitive actions will be listed here as they happen." />
              </div>
            }
          >
            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>When</th>
                      <th>User</th>
                      <th>Action</th>
                      <th>Target</th>
                      <th class="cell-hide-sm">Repository</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={d().entries}>
                      {(entry) => (
                        <tr>
                          <td class="cell-dim nowrap" title={entry.created_at}>
                            {timeAgo(entry.created_at)}
                          </td>
                          <td>
                            <div class="row">
                              <div class="avatar" style={{ width: '24px', height: '24px', 'font-size': '0.56rem' }}>
                                {initials(entry.username ?? '?')}
                              </div>
                              <span class="small" style={{ color: 'var(--ink)' }}>
                                {entry.username ?? '—'}
                              </span>
                            </div>
                          </td>
                          <td>
                            <span class={actionChip(entry.action)}>{entry.action}</span>
                          </td>
                          <td class="cell-mono cell-muted truncate" style={{ 'max-width': '260px' }}>
                            {entry.target || '—'}
                          </td>
                          <td class="cell-mono cell-dim cell-hide-sm">{entry.repository || '—'}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
              <div class="pagination">
                <span class="pagination-info">Page {d().page}</span>
                <div class="pagination-nav">
                  <button class="btn btn-ghost btn-sm" disabled={page() <= 1} onClick={() => setPage((p) => p - 1)}>
                    <Icon name="chevron-left" size={14} />
                    Newer
                  </button>
                  <button
                    class="btn btn-ghost btn-sm"
                    disabled={d().entries.length < pageSize}
                    onClick={() => setPage((p) => p + 1)}
                  >
                    Older
                    <Icon name="chevron-right" size={14} />
                  </button>
                </div>
              </div>
            </div>
          </Show>
        )}
      </Show>
    </div>
  );
}
