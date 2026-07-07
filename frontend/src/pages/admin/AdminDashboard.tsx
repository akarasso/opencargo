import { For, Show } from 'solid-js';
import { A } from '@solidjs/router';
import Icon from '../../components/Icon.tsx';
import CountUp from '../../components/CountUp.tsx';
import RoleBadge from '../../components/RoleBadge.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { FormatTag, LoadError, StatsSkeleton, VisibilityChip } from '../../components/bits.tsx';
import { fetchAudit, fetchDashboard, fetchHealthReady, fetchRepositories, fetchUsers } from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';
import { timeAgo } from '../../core/format.ts';
import { wsStatus } from '../../core/ws.ts';

export default function AdminDashboard() {
  return (
    <RequireAdmin>
      <AdminDashboardInner />
    </RequireAdmin>
  );
}

function AdminDashboardInner() {
  const [dashboard] = createLiveResource(fetchDashboard, [
    'package.published',
    'package.promoted',
    'registry.changed',
    'repositories.changed',
  ]);
  const [repos] = createLiveResource(fetchRepositories, ['repositories.changed']);
  const [users] = createLiveResource(fetchUsers, ['audit.entry', 'permissions.changed'], { debounce: 600 });
  const [health] = createLiveResource(fetchHealthReady, [], { debounce: 1000 });
  const [audit] = createLiveResource(() => fetchAudit(1, 8), ['audit.entry'], { debounce: 400 });

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Overview</h1>
          <p class="page-sub">Registry operations at a glance — everything here updates live.</p>
        </div>
        <div class="row">
          <span class={`chip ${health()?.status === 'ok' ? 'chip-ok' : 'chip-danger'}`}>
            <Icon name={health()?.status === 'ok' ? 'check-circle' : 'alert-triangle'} size={12} />
            database {health()?.status === 'ok' ? 'healthy' : 'degraded'}
          </span>
          <span class={`chip ${wsStatus() === 'online' ? 'chip-ok' : 'chip-neutral'}`}>
            <Icon name="zap" size={12} />
            events {wsStatus()}
          </span>
        </div>
      </div>

      <Show when={dashboard.error}>
        <LoadError what="registry stats" />
      </Show>

      <div class="stagger">
        <Show when={dashboard()} fallback={<StatsSkeleton />}>
          {(d) => (
            <section class="stats-grid section">
              <div class="stat" style={{ '--stat-tint': 'var(--accent)' }}>
                <div class="stat-head">
                  <span class="stat-label">Packages</span>
                  <Icon name="package" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_packages} />
                </div>
                <div class="stat-foot">{d().total_versions.toLocaleString()} versions total</div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--ok)' }}>
                <div class="stat-head">
                  <span class="stat-label">Downloads</span>
                  <Icon name="download" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_downloads} />
                </div>
                <div class="stat-foot">served from this node</div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--steel)' }}>
                <div class="stat-head">
                  <span class="stat-label">Users</span>
                  <Icon name="users" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={users()?.length ?? 0} />
                </div>
                <div class="stat-foot">
                  {users()?.filter((u) => u.role === 'admin').length ?? 0} admin ·{' '}
                  {users()?.filter((u) => u.role === 'publisher').length ?? 0} publisher
                </div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--fmt-go)' }}>
                <div class="stat-head">
                  <span class="stat-label">Repositories</span>
                  <Icon name="database" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_repos} />
                </div>
                <div class="stat-foot">
                  {repos()?.repositories.filter((r) => r.visibility === 'private').length ?? 0} private
                </div>
              </div>
            </section>
          )}
        </Show>

        <section class="section dashboard-cols">
          {/* Recent admin activity */}
          <div class="feed-card">
            <div class="feed-head">
              <div class="row">
                <Icon name="history" size={15} />
                <span class="section-title">Recent activity</span>
              </div>
              <A href="/admin/audit" class="btn btn-quiet btn-sm">
                Full audit log
                <Icon name="arrow-right" size={13} />
              </A>
            </div>
            <Show
              when={audit()}
              fallback={
                <ul class="feed">
                  <For each={[0, 1, 2, 3]}>
                    {() => (
                      <li class="feed-row">
                        <div class="skeleton skeleton-text" style={{ width: '64px' }} />
                        <div class="skeleton skeleton-text grow" style={{ 'max-width': '55%' }} />
                      </li>
                    )}
                  </For>
                </ul>
              }
            >
              {(a) => (
                <Show
                  when={a().entries.length > 0}
                  fallback={<div class="empty" style={{ padding: '28px' }}><div class="empty-text">No admin actions recorded yet.</div></div>}
                >
                  <ul class="feed">
                    <For each={a().entries}>
                      {(e) => (
                        <li class="feed-row">
                          <span class="feed-time" title={e.created_at}>
                            {timeAgo(e.created_at)}
                          </span>
                          <div class="feed-main">
                            <span class="mono small" style={{ color: 'var(--ink)' }}>
                              {e.username ?? '—'}
                            </span>
                            <span class="chip chip-neutral">{e.action}</span>
                            <Show when={e.target}>
                              <span class="dim small mono truncate">{e.target}</span>
                            </Show>
                          </div>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              )}
            </Show>
          </div>

          {/* Repositories + shortcuts */}
          <div class="col" style={{ gap: '16px' }}>
            <div class="card">
              <div class="feed-head">
                <span class="section-title">Repositories</span>
                <A href="/admin/repositories" class="btn btn-quiet btn-sm">
                  Manage
                </A>
              </div>
              <div style={{ padding: '6px 8px 8px' }}>
                <For each={(repos()?.repositories ?? []).slice(0, 6)}>
                  {(repo) => (
                    <div class="row" style={{ padding: '7px 10px', 'justify-content': 'space-between' }}>
                      <span class="mono small grow truncate" style={{ color: 'var(--ink)' }}>
                        {repo.name}
                      </span>
                      <FormatTag format={repo.format} />
                      <VisibilityChip visibility={repo.visibility} />
                    </div>
                  )}
                </For>
              </div>
            </div>

            <div class="card card-pad col" style={{ gap: '8px' }}>
              <div class="section-title" style={{ 'margin-bottom': '4px' }}>
                Shortcuts
              </div>
              <A class="btn btn-ghost" href="/admin/users" style={{ 'justify-content': 'flex-start' }}>
                <Icon name="users" size={14} />
                Manage users & access
              </A>
              <A class="btn btn-ghost" href="/admin/webhooks" style={{ 'justify-content': 'flex-start' }}>
                <Icon name="webhook" size={14} />
                Configure webhooks
              </A>
              <A class="btn btn-ghost" href="/admin/system" style={{ 'justify-content': 'flex-start' }}>
                <Icon name="settings" size={14} />
                System & metrics
              </A>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}
