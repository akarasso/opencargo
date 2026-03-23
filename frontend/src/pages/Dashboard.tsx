import { createResource, Show, For } from 'solid-js';
import { A } from '@solidjs/router';
import { fetchDashboard } from '../lib/api.ts';
import LoadingSpinner from '../components/LoadingSpinner.tsx';

export default function Dashboard() {
  const [data] = createResource(fetchDashboard);

  return (
    <>
      <Show when={data.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={data.error}>
        <div class="alert alert-error">Failed to load dashboard data.</div>
      </Show>

      <Show when={data()}>
        {(d) => (
          <div style={{ display: 'flex', "flex-direction": 'column', gap: '2rem' }}>
            {/* Stat Grid -- matches Stitch: grid-cols-4 */}
            <section style={{ display: 'grid', "grid-template-columns": 'repeat(4, 1fr)', gap: '1.5rem' }}>
              {/* Stat Card: Packages */}
              <div class="stat-card" style={{ "border-left": "none", "border-radius": "0.75rem" }}>
                <div class="stat-card-decor" style={{ background: 'rgba(123, 231, 249, 0.05)' }} />
                <div style={{ position: 'relative', "z-index": 1 }}>
                  <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1rem' }}>
                    <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-primary)" }}>Packages</span>
                    <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)', "font-size": '20px' }}>inventory_2</span>
                  </div>
                  <div style={{ "font-size": "1.875rem", "font-family": "var(--font-headline)", "font-weight": "700", "letter-spacing": "-0.05em", color: "var(--clr-on-surface)" }}>{d().total_packages.toLocaleString()}</div>
                  <div style={{ "margin-top": "0.5rem", "font-size": "0.625rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)", opacity: 0.6 }}>REGISTRY_UPTIME: 100%</div>
                </div>
              </div>

              {/* Stat Card: Versions */}
              <div class="stat-card" style={{ "border-left": "none", "border-radius": "0.75rem" }}>
                <div class="stat-card-decor" style={{ background: 'rgba(16, 213, 255, 0.05)' }} />
                <div style={{ position: 'relative', "z-index": 1 }}>
                  <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1rem' }}>
                    <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-secondary)" }}>Versions</span>
                    <span class="material-symbols-outlined" style={{ color: 'var(--clr-secondary)', "font-size": '20px' }}>history_edu</span>
                  </div>
                  <div style={{ "font-size": "1.875rem", "font-family": "var(--font-headline)", "font-weight": "700", "letter-spacing": "-0.05em", color: "var(--clr-on-surface)" }}>{d().total_versions.toLocaleString()}</div>
                  <div style={{ "margin-top": "0.5rem", "font-size": "0.625rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)", opacity: 0.6 }}>DELTA_SYNC: ACTIVE</div>
                </div>
              </div>

              {/* Stat Card: Downloads */}
              <div class="stat-card" style={{ "border-left": "none", "border-radius": "0.75rem" }}>
                <div class="stat-card-decor" style={{ background: 'rgba(59, 175, 193, 0.05)' }} />
                <div style={{ position: 'relative', "z-index": 1 }}>
                  <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1rem' }}>
                    <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-primary-container)" }}>Downloads</span>
                    <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary-container)', "font-size": '20px' }}>download</span>
                  </div>
                  <div style={{ "font-size": "1.875rem", "font-family": "var(--font-headline)", "font-weight": "700", "letter-spacing": "-0.05em", color: "var(--clr-on-surface)" }}>{d().total_downloads.toLocaleString()}</div>
                  <div style={{ "margin-top": "0.5rem", "font-size": "0.625rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)", opacity: 0.6 }}>THROUGHPUT: 1.2GB/S</div>
                </div>
              </div>

              {/* Stat Card: Repos */}
              <div class="stat-card" style={{ "border-left": "none", "border-radius": "0.75rem" }}>
                <div class="stat-card-decor" style={{ background: 'rgba(138, 184, 255, 0.05)' }} />
                <div style={{ position: 'relative', "z-index": 1 }}>
                  <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1rem' }}>
                    <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-tertiary)" }}>Repos</span>
                    <span class="material-symbols-outlined" style={{ color: 'var(--clr-tertiary)', "font-size": '20px' }}>hub</span>
                  </div>
                  <div style={{ "font-size": "1.875rem", "font-family": "var(--font-headline)", "font-weight": "700", "letter-spacing": "-0.05em", color: "var(--clr-on-surface)" }}>{d().total_repos}</div>
                  <div style={{ "margin-top": "0.5rem", "font-size": "0.625rem", "font-family": "var(--font-mono)", color: "var(--clr-on-surface-variant)", opacity: 0.6 }}>ENDPOINT: GLOBAL_EDGE</div>
                </div>
              </div>
            </section>

            {/* Data & Health Section -- matches Stitch: grid-cols-12 */}
            <section class="dashboard-bottom">
              {/* Recent Activity Table */}
              <div style={{ background: 'var(--clr-surface-container)', "border-radius": '0.75rem', overflow: 'hidden', "box-shadow": 'var(--shadow-2xl)' }}>
                <div style={{ padding: '1.5rem 2rem', display: 'flex', "align-items": 'center', "justify-content": 'space-between' }}>
                  <h2 style={{ "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', "font-size": '1.25rem', "margin-bottom": '0' }}>Recent Activity</h2>
                  <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface-variant)', "letter-spacing": '0.2em', "text-transform": 'uppercase', background: 'var(--clr-surface-container-high)', padding: '0.25rem 0.75rem', "border-radius": 'var(--radius-sm)' }}>STREAM: 0x981A</div>
                </div>
                <div style={{ "overflow-x": 'auto' }}>
                  <table class="data-table">
                    <thead>
                      <tr>
                        <th>Package Name</th>
                        <th>Version</th>
                        <th style={{ "text-align": 'right' }}>Date</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={d().recent_versions}>
                        {(rv) => (
                          <tr>
                            <td style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                              <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary-container)', "font-size": '14px' }}>deployed_code</span>
                              <A href={`/packages/${rv.package_name}`} style={{ "font-weight": '500', "font-size": '0.875rem', color: 'var(--clr-on-surface)', "text-decoration": 'none' }}>{rv.package_name}</A>
                            </td>
                            <td>
                              <span style={{ background: 'rgba(123, 231, 249, 0.1)', color: 'var(--clr-primary)', padding: '0.25rem 0.5rem', "border-radius": 'var(--radius-sm)', "font-size": '0.625rem', "font-family": 'var(--font-mono)', "font-weight": '700' }}>{rv.version}</span>
                            </td>
                            <td style={{ "text-align": 'right', "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>{rv.published_at}</td>
                          </tr>
                        )}
                      </For>
                      <Show when={d().recent_versions.length === 0}>
                        <tr>
                          <td colspan="3" style={{ "text-align": 'center', padding: '2rem', color: 'var(--clr-on-surface-variant)' }}>No recent activity</td>
                        </tr>
                      </Show>
                    </tbody>
                  </table>
                </div>
              </div>

              {/* System Health Column -- matches Stitch */}
              <div style={{ display: 'flex', "flex-direction": 'column' }}>
                <div style={{ background: 'var(--clr-surface-container)', "border-radius": '0.75rem', padding: '2rem', display: 'flex', "flex-direction": 'column', height: '100%', "box-shadow": 'var(--shadow-2xl)' }}>
                  <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '2rem' }}>
                    <h2 style={{ "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', "font-size": '1.25rem', "margin-bottom": '0' }}>System Health</h2>
                    <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)' }}>sensors</span>
                  </div>
                  <div style={{ display: 'flex', "flex-direction": 'column', gap: '2rem', flex: '1' }}>
                    {/* Memory */}
                    <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.75rem' }}>
                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
                        <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-on-surface-variant)" }}>Memory Usage</span>
                        <span style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-primary)' }}>64%</span>
                      </div>
                      <div class="health-bar-container">
                        <div class="health-bar-fill" style={{ width: '64%' }} />
                      </div>
                    </div>
                    {/* CPU */}
                    <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.75rem' }}>
                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
                        <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-on-surface-variant)" }}>CPU Load</span>
                        <span style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-primary)' }}>21%</span>
                      </div>
                      <div class="health-bar-container">
                        <div class="health-bar-fill" style={{ width: '21%' }} />
                      </div>
                    </div>
                    {/* Network */}
                    <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.75rem' }}>
                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
                        <span style={{ "font-family": "var(--font-label)", "text-transform": "uppercase", "letter-spacing": "0.2em", "font-size": "0.625rem", "font-weight": "700", color: "var(--clr-on-surface-variant)" }}>Network Latency</span>
                        <span style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-primary)' }}>12ms</span>
                      </div>
                      <div class="health-bar-container">
                        <div class="health-bar-fill" style={{ width: '8%' }} />
                      </div>
                    </div>
                  </div>
                  {/* Security status */}
                  <div style={{ "margin-top": 'auto', "padding-top": '2rem' }}>
                    <div style={{ background: 'var(--clr-surface-container-lowest)', padding: '1rem', "border-radius": '0.5rem', border: '1px solid rgba(67, 72, 78, 0.1)' }}>
                      <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                        <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)', "font-size": '14px', "font-variation-settings": "'FILL' 1" }}>security</span>
                        <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface)', "line-height": '1.4' }}>
                          <p style={{ "text-transform": 'uppercase', "font-weight": '700', "letter-spacing": '0.05em', "margin-bottom": '0' }}>Security Protocol: Active</p>
                          <p style={{ color: 'var(--clr-on-surface-variant)', opacity: 0.6, "margin-bottom": '0' }}>SHA-256 VERIFIED</p>
                        </div>
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            </section>
          </div>
        )}
      </Show>
    </>
  );
}
