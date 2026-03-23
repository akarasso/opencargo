import { createResource, For, Show } from 'solid-js';
import { fetchRepositories } from '../../lib/api.ts';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';
import EmptyState from '../../components/EmptyState.tsx';

export default function Repositories() {
  const [repos] = createResource(fetchRepositories);

  return (
    <>
      <Show when={repos.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={repos.error}>
        <div class="alert alert-error">Failed to load repositories.</div>
      </Show>

      <Show when={repos()}>
        {(r) => (
          <Show
            when={r().repositories.length > 0}
            fallback={
              <EmptyState
                title="No repositories"
                text="No repositories have been configured yet."
              />
            }
          >
            <div style={{ display: 'flex', "flex-direction": 'column', gap: '2.5rem' }}>
              {/* Stat Grid -- matches Stitch repos page: 3 stat cards */}
              <div style={{ display: 'grid', "grid-template-columns": 'repeat(3, 1fr)', gap: '1.5rem' }}>
                {/* Total */}
                <div class="repo-stat-card">
                  <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.05 }}>
                    <span class="material-symbols-outlined" style={{ "font-size": '4rem' }}>inventory_2</span>
                  </div>
                  <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
                    <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)' }}>Registry.Total</span>
                    <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
                      <span style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-primary)' }}>{r().repositories.length}</span>
                      <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-label)', color: 'var(--clr-outline)' }}>Modules</span>
                    </div>
                  </div>
                  <div style={{ "margin-top": '1rem', height: '4px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                    <div style={{ height: '100%', width: '100%', background: 'var(--clr-primary)', "box-shadow": '0 0 8px rgba(123, 231, 249, 0.5)' }} />
                  </div>
                </div>

                {/* Hosted */}
                <div class="repo-stat-card">
                  <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.05 }}>
                    <span class="material-symbols-outlined" style={{ "font-size": '4rem' }}>cloud_done</span>
                  </div>
                  <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
                    <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)' }}>Registry.Active</span>
                    <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
                      <span style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-secondary)' }}>{r().repositories.length}</span>
                      <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-label)', color: 'var(--clr-outline)' }}>Internal</span>
                    </div>
                  </div>
                  <div style={{ "margin-top": '1rem', height: '4px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                    <div style={{ height: '100%', width: '100%', background: 'var(--clr-secondary)', "box-shadow": '0 0 8px rgba(16, 213, 255, 0.5)' }} />
                  </div>
                </div>

                {/* Format */}
                <div class="repo-stat-card">
                  <div style={{ position: 'absolute', top: 0, right: 0, padding: '1rem', opacity: 0.05 }}>
                    <span class="material-symbols-outlined" style={{ "font-size": '4rem' }}>hub</span>
                  </div>
                  <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
                    <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)' }}>Registry.Format</span>
                    <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
                      <span style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-tertiary)' }}>npm</span>
                    </div>
                  </div>
                  <div style={{ "margin-top": '1rem', height: '4px', width: '100%', background: 'var(--clr-surface-container-high)', "border-radius": '9999px', overflow: 'hidden' }}>
                    <div style={{ height: '100%', width: '40%', background: 'var(--clr-tertiary)', "box-shadow": '0 0 8px rgba(138, 184, 255, 0.5)' }} />
                  </div>
                </div>
              </div>

              {/* Table Container -- matches Stitch repos page */}
              <div class="data-table-wrapper">
                <div style={{ padding: '1.5rem 2rem', display: 'flex', "justify-content": 'space-between', "align-items": 'center', "border-bottom": '1px solid rgba(255, 255, 255, 0.05)', background: 'rgba(26, 32, 39, 0.3)' }}>
                  <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                    <div style={{ width: '6px', height: '24px', background: 'var(--clr-primary)', "border-radius": '9999px' }} />
                    <h3 style={{ "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', color: 'var(--clr-on-surface)', "margin-bottom": '0' }}>ACTIVE REPOSITORIES</h3>
                  </div>
                </div>
                <div style={{ "overflow-x": 'auto' }}>
                  <table class="data-table">
                    <thead>
                      <tr>
                        <th>Name</th>
                        <th>Status</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={r().repositories}>
                        {(repo) => (
                          <tr>
                            <td>
                              <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                                <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary-dim)', "font-size": '18px' }}>folder</span>
                                <span style={{ "font-weight": '500', color: 'var(--clr-on-surface)' }}>{repo.name}</span>
                              </div>
                            </td>
                            <td>
                              <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem' }}>
                                <div style={{ width: '8px', height: '8px', "border-radius": '50%', background: 'var(--clr-primary)', "box-shadow": '0 0 8px rgba(123, 231, 249, 0.8)' }} class="status-led-animated" />
                                <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', color: 'var(--clr-primary)' }}>Live</span>
                              </div>
                            </td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                </div>
                <div style={{ padding: '1rem 2rem', "border-top": '1px solid rgba(255, 255, 255, 0.05)', display: 'flex', "justify-content": 'space-between', "align-items": 'center', background: 'rgba(26, 32, 39, 0.1)' }}>
                  <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)' }}>Showing {r().repositories.length} repositories</span>
                </div>
              </div>

              {/* Info banner */}
              <div class="info-banner">
                <span class="material-symbols-outlined">info</span>
                <div>
                  <h4 class="info-banner-title">System Maintenance</h4>
                  <p class="info-banner-text">
                    Repository configuration is managed via the server configuration file. Edit the config and restart the server to add or modify repositories.
                  </p>
                </div>
              </div>
            </div>
          </Show>
        )}
      </Show>
    </>
  );
}
