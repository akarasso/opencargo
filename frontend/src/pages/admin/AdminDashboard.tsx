import { createResource, Show, For } from 'solid-js';
import { A } from '@solidjs/router';
import { fetchDashboard, fetchRepositories, fetchHealthReady } from '../../lib/api.ts';
import StatsCard from '../../components/StatsCard.tsx';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';

export default function AdminDashboard() {
  const [dashboard] = createResource(fetchDashboard);
  const [repos] = createResource(fetchRepositories);
  const [health] = createResource(fetchHealthReady);

  return (
    <>
      {/* Technical Breadcrumb -- matches Stitch admin-dashboard */}
      <div style={{ "margin-bottom": '2rem', display: 'flex', "align-items": 'center', "justify-content": 'space-between' }}>
        <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem' }}>
          <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)' }}>OpenCargo</span>
          <span style={{ color: 'var(--clr-primary)', "font-size": '0.625rem' }}>/</span>
          <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-primary)' }}>Overview</span>
        </div>
        <div style={{ display: 'flex', "align-items": 'center', gap: '1rem', "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)' }}>
          <span>LATENCY: 12MS</span>
          <span style={{ width: '4px', height: '4px', background: 'var(--clr-primary)', "border-radius": '50%' }} class="status-led-animated" />
          <span>Uptime: 99.98%</span>
        </div>
      </div>

      <Show when={dashboard.loading || repos.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={dashboard.error}>
        <div class="alert alert-error">Failed to load dashboard data.</div>
      </Show>

      <Show when={dashboard()}>
        {(d) => (
          <>
            {/* 4 Stat Cards Row -- matches Stitch admin-dashboard */}
            <div style={{ display: 'grid', "grid-template-columns": 'repeat(4, 1fr)', gap: '1.5rem', "margin-bottom": '2.5rem' }}>
              {/* Packages */}
              <div class="stat-card" style={{ "border-left": 'none', "border-radius": '0.75rem' }}>
                <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.1 }}>
                  <span class="material-symbols-outlined" style={{ "font-size": '2.5rem' }}>package_2</span>
                </div>
                <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Total Packages</div>
                <div style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{d().total_packages.toLocaleString()}</div>
                <div class="stat-card-trend">
                  <span>+12%</span>
                  <div class="stat-card-trend-line" />
                </div>
              </div>

              {/* Versions */}
              <div class="stat-card" style={{ "border-left": 'none', "border-radius": '0.75rem' }}>
                <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.1 }}>
                  <span class="material-symbols-outlined" style={{ "font-size": '2.5rem' }}>history</span>
                </div>
                <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Build Versions</div>
                <div style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{d().total_versions.toLocaleString()}</div>
                <div class="stat-card-trend">
                  <span>Active Build</span>
                  <div class="stat-card-trend-line" />
                </div>
              </div>

              {/* Downloads */}
              <div class="stat-card" style={{ "border-left": 'none', "border-radius": '0.75rem' }}>
                <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.1 }}>
                  <span class="material-symbols-outlined" style={{ "font-size": '2.5rem' }}>download</span>
                </div>
                <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Global Downloads</div>
                <div style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{d().total_downloads.toLocaleString()}</div>
                <div class="stat-card-trend">
                  <span style={{ color: 'var(--clr-secondary)' }}>Peak Traffic</span>
                  <div class="stat-card-trend-line" />
                </div>
              </div>

              {/* Repos */}
              <div class="stat-card" style={{ "border-left": 'none', "border-radius": '0.75rem' }}>
                <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.1 }}>
                  <span class="material-symbols-outlined" style={{ "font-size": '2.5rem' }}>database</span>
                </div>
                <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Repositories</div>
                <div style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{d().total_repos}</div>
                <div class="stat-card-trend">
                  <span>Synchronized</span>
                  <div class="stat-card-trend-line" />
                </div>
              </div>
            </div>

            {/* Two-Column Section -- matches Stitch */}
            <div style={{ display: 'grid', "grid-template-columns": '2fr 1fr', gap: '2rem' }}>
              {/* Recent Activity Table */}
              <div style={{ background: 'var(--clr-surface-container)', "border-radius": '0.75rem', overflow: 'hidden' }}>
                <div style={{ padding: '1.5rem', "border-bottom": '1px solid rgba(255, 255, 255, 0.05)', display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                  <h2 style={{ "font-family": 'var(--font-headline)', "font-size": '0.875rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', "font-weight": '700', "margin-bottom": '0' }}>Recent Activity</h2>
                  <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', color: 'var(--clr-on-surface-variant)' }}>Live_Feed_0x24</span>
                </div>
                <div style={{ "overflow-x": 'auto' }}>
                  <table class="data-table">
                    <thead>
                      <tr>
                        <th>Package Name</th>
                        <th>Version</th>
                        <th>Date</th>
                        <th>Status</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={d().recent_versions}>
                        {(rv) => (
                          <tr>
                            <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-on-surface)' }}>{rv.package_name}</td>
                            <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-primary)' }}>{rv.version}</td>
                            <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>{rv.published_at}</td>
                            <td>
                              <span style={{ display: 'flex', "align-items": 'center', gap: '0.5rem', "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', color: 'var(--clr-primary)' }}>
                                <span style={{ width: '6px', height: '6px', "border-radius": '50%', background: 'var(--clr-primary)' }} />
                                Deployed
                              </span>
                            </td>
                          </tr>
                        )}
                      </For>
                      <Show when={d().recent_versions.length === 0}>
                        <tr><td colspan="4" style={{ "text-align": 'center', padding: '2rem', color: 'var(--clr-on-surface-variant)' }}>No recent activity</td></tr>
                      </Show>
                    </tbody>
                  </table>
                </div>
              </div>

              {/* System Health Column -- matches Stitch */}
              <div style={{ background: 'var(--clr-surface-container)', "border-radius": '0.75rem', display: 'flex', "flex-direction": 'column', height: '100%', border: '1px solid rgba(255, 255, 255, 0.05)' }}>
                <div style={{ padding: '1.5rem', "border-bottom": '1px solid rgba(255, 255, 255, 0.05)' }}>
                  <h2 style={{ "font-family": 'var(--font-headline)', "font-size": '0.875rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', "font-weight": '700', "margin-bottom": '0.25rem' }}>System Health</h2>
                  <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', color: 'var(--clr-on-surface-variant)', "margin-top": '0.25rem' }}>NODE_ID: OC-ALPHA-01</p>
                </div>
                <div style={{ padding: '1.5rem', display: 'flex', "flex-direction": 'column', gap: '2rem', flex: '1' }}>
                  <div>
                    <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end', "margin-bottom": '0.5rem' }}>
                      <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>Memory Usage</span>
                      <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>64%</span>
                    </div>
                    <div style={{ height: '6px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                      <div class="kinetic-gradient" style={{ height: '100%', width: '64%' }} />
                    </div>
                  </div>
                  <div>
                    <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end', "margin-bottom": '0.5rem' }}>
                      <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>CPU Load</span>
                      <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>21%</span>
                    </div>
                    <div style={{ height: '6px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                      <div class="kinetic-gradient" style={{ height: '100%', width: '21%' }} />
                    </div>
                  </div>
                  <div>
                    <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end', "margin-bottom": '0.5rem' }}>
                      <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>Network Latency</span>
                      <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>12ms</span>
                    </div>
                    <div style={{ height: '6px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                      <div class="kinetic-gradient" style={{ height: '100%', width: '45%' }} />
                    </div>
                  </div>
                </div>
                {/* Security Status */}
                <div style={{ padding: '1.5rem', "margin-top": 'auto' }}>
                  <div style={{ background: 'rgba(59, 175, 193, 0.1)', border: '1px solid rgba(123, 231, 249, 0.2)', padding: '1rem', "border-radius": '0.5rem', display: 'flex', "align-items": 'center', "justify-content": 'space-between' }}>
                    <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                      <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)' }}>shield</span>
                      <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "font-weight": '700', color: 'var(--clr-primary)' }}>Security Protocol: Active</span>
                    </div>
                    <span style={{ width: '8px', height: '8px', "border-radius": '50%', background: 'var(--clr-primary)' }} class="status-led-animated" />
                  </div>
                </div>
              </div>
            </div>
          </>
        )}
      </Show>
    </>
  );
}
