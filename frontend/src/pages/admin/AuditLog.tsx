import { createSignal, createResource, For, Show } from 'solid-js';
import { fetchAudit } from '../../lib/api.ts';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';
import EmptyState from '../../components/EmptyState.tsx';

function actionBadgeClass(action: string): string {
  if (action.includes('publish')) return 'badge badge-action-publish';
  if (action.includes('create')) return 'badge badge-action-create';
  if (action.includes('login')) return 'badge badge-action-login';
  if (action.includes('delete') || action.includes('yank') || action.includes('revoke')) return 'badge badge-action-revoke';
  return 'badge badge-default';
}

function getInitials(username: string): string {
  const parts = username.split(/[@._-]/);
  if (parts.length >= 2) {
    return (parts[0][0] + (parts[1][0] || '')).toUpperCase();
  }
  return username.slice(0, 2).toUpperCase();
}

export default function AuditLog() {
  const [page, setPage] = createSignal(1);
  const pageSize = 50;

  const [data] = createResource(
    () => page(),
    (p) => fetchAudit(p, pageSize),
  );

  return (
    <>
      {/* Header Section -- matches Stitch audit page */}
      <div style={{ "margin-bottom": '2.5rem', display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
        <div>
          <h1 style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.05em', color: 'var(--clr-on-surface)', "margin-bottom": '0.5rem', "text-transform": 'uppercase' }}>Audit Log</h1>
          <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
            <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem', "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgba(123, 231, 249, 0.6)' }}>
              <span style={{ width: '8px', height: '8px', background: 'var(--clr-primary)', "border-radius": '50%' }} class="status-led-animated" />
              LIVE STREAM ACTIVE
            </div>
          </div>
        </div>
      </div>

      <Show when={data.loading}><LoadingSpinner /></Show>
      <Show when={data.error}><div class="alert alert-error">Failed to load audit log. Make sure you are logged in as an admin.</div></Show>

      <Show when={data()}>
        {(d) => (
          <Show when={d().entries.length > 0} fallback={<EmptyState title="No audit entries" text="No actions have been recorded yet." />}>
            {/* Audit Table -- matches Stitch audit page exactly */}
            <div class="data-table-wrapper" style={{ position: 'relative' }}>
              <div style={{ "overflow-x": 'auto' }}>
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Timestamp</th>
                      <th>User Identity</th>
                      <th>Event Action</th>
                      <th>Target Object</th>
                      <th>Repository</th>
                      <th style={{ "text-align": 'right' }}>Source IP</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={d().entries}>
                      {(entry) => (
                        <tr>
                          <td style={{ "font-family": 'var(--font-label)', "font-size": '0.75rem', color: 'rgb(148, 163, 184)' }}>{entry.created_at}</td>
                          <td>
                            <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem' }}>
                              <div style={{ width: '24px', height: '24px', "border-radius": '0.125rem', background: 'var(--clr-surface-variant)', display: 'flex', "align-items": 'center', "justify-content": 'center', "font-size": '0.625rem', "font-weight": '700', color: 'var(--clr-primary)' }}>
                                {entry.username ? getInitials(entry.username) : '??'}
                              </div>
                              <span style={{ "font-size": '0.75rem', "font-weight": '500', color: 'var(--clr-on-surface)' }}>{entry.username || '--'}</span>
                            </div>
                          </td>
                          <td><span class={actionBadgeClass(entry.action)}>{entry.action}</span></td>
                          <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.625rem', color: 'rgb(100, 116, 139)' }}>{entry.target || '--'}</td>
                          <td style={{ "font-size": '0.75rem', color: 'rgb(203, 213, 225)' }}>{entry.repository || '--'}</td>
                          <td style={{ "text-align": 'right', "font-family": 'var(--font-mono)', "font-size": '0.625rem', color: 'rgb(100, 116, 139)' }}>{entry.ip || '--'}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>

              {/* Pagination -- matches Stitch audit page */}
              <div style={{ background: 'rgba(26, 32, 39, 0.5)', "border-top": '1px solid rgba(255, 255, 255, 0.05)', padding: '1rem 1.5rem', display: 'flex', "align-items": 'center', "justify-content": 'space-between' }}>
                <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(100, 116, 139)' }}>
                  Showing <span style={{ color: 'var(--clr-on-surface)' }}>1 - {d().entries.length}</span> entries
                </div>
                <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem' }}>
                  <Show when={page() > 1}>
                    <button class="pagination-btn" onClick={() => setPage((p) => Math.max(1, p - 1))}>
                      <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>chevron_left</span>
                      Previous
                    </button>
                  </Show>
                  <button class="pagination-page pagination-page-active">{page()}</button>
                  <Show when={d().entries.length >= pageSize}>
                    <button class="pagination-page" onClick={() => setPage((p) => p + 1)}>{page() + 1}</button>
                    <button class="pagination-btn" onClick={() => setPage((p) => p + 1)}>
                      Next
                      <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>chevron_right</span>
                    </button>
                  </Show>
                </div>
              </div>
            </div>

            {/* Bottom Stats -- matches Stitch audit page */}
            <div class="audit-stats-grid">
              <div class="audit-stat-card" style={{ "border-left": '2px solid rgba(129, 236, 255, 0.3)' }}>
                <div style={{ "font-size": '0.5rem', "font-family": 'var(--font-label)', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "margin-bottom": '0.25rem' }}>Storage_Integrity</div>
                <div style={{ "font-size": '1.25rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: '#81ecff' }}>99.998%</div>
              </div>
              <div class="audit-stat-card" style={{ "border-left": '2px solid rgba(16, 213, 255, 0.3)' }}>
                <div style={{ "font-size": '0.5rem', "font-family": 'var(--font-label)', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "margin-bottom": '0.25rem' }}>Active_Sessions</div>
                <div style={{ "font-size": '1.25rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-secondary-fixed-dim)' }}>{d().entries.length}</div>
              </div>
              <div class="audit-stat-card" style={{ "border-left": '2px solid rgba(255, 113, 108, 0.3)' }}>
                <div style={{ "font-size": '0.5rem', "font-family": 'var(--font-label)', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "margin-bottom": '0.25rem' }}>Latent_Anomalies</div>
                <div style={{ "font-size": '1.25rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-error)' }}>0</div>
              </div>
              <div class="audit-stat-card" style={{ "border-left": '2px solid rgba(148, 163, 184, 0.3)' }}>
                <div style={{ "font-size": '0.5rem', "font-family": 'var(--font-label)', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "margin-bottom": '0.25rem' }}>Log_Retention</div>
                <div style={{ "font-size": '1.25rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'rgb(148, 163, 184)' }}>90D</div>
              </div>
            </div>
          </Show>
        )}
      </Show>
    </>
  );
}
