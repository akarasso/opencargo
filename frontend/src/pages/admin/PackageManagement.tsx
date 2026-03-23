import { createSignal, createResource, For, Show } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import { fetchPackages, fetchRepositories, promotePackage } from '../../lib/api.ts';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';
import EmptyState from '../../components/EmptyState.tsx';

function paramStr(val: string | string[] | undefined): string {
  if (Array.isArray(val)) return val[0] ?? '';
  return val ?? '';
}

/** Guess format from package name or repo name. */
function guessFormat(name: string): string {
  if (name.startsWith('@') || name.includes('/')) return 'npm';
  if (name.includes('github.com/') || name.includes('golang.org/')) return 'go';
  if (name.includes(':') || name.includes('sha256')) return 'oci';
  return 'cargo';
}

/** Return badge class for format. */
function formatBadgeClass(fmt: string): string {
  switch (fmt) {
    case 'npm': return 'format-badge-npm';
    case 'cargo': return 'format-badge-cargo';
    case 'oci': return 'format-badge-oci';
    case 'go': return 'format-badge-go';
    default: return 'format-badge-npm';
  }
}

export default function PackageManagement() {
  const [searchParams, setSearchParams] = useSearchParams();

  const query = () => paramStr(searchParams.q);
  const repoFilter = () => paramStr(searchParams.repo);
  const page = () => parseInt(paramStr(searchParams.page) || '1', 10) || 1;

  const [inputValue, setInputValue] = createSignal(query());

  const [repos] = createResource(fetchRepositories);
  const [data, { refetch }] = createResource(
    () => ({ q: query(), repo: repoFilter(), page: page() }),
    fetchPackages,
  );

  // Promote modal state
  const [promoteTarget, setPromoteTarget] = createSignal<{ name: string; version: string } | null>(null);
  const [promoteFrom, setPromoteFrom] = createSignal('');
  const [promoteTo, setPromoteTo] = createSignal('');
  const [promoteLoading, setPromoteLoading] = createSignal(false);
  const [promoteMsg, setPromoteMsg] = createSignal<string | null>(null);

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;

  function handleInput(value: string) {
    setInputValue(value);
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      setSearchParams({ q: value || undefined, page: '1' });
    }, 300);
  }

  function handleRepoChange(value: string) {
    setSearchParams({ repo: value || undefined, page: '1' });
  }

  function goToPage(p: number) {
    setSearchParams({ page: String(p) });
  }

  function openPromote(name: string, version: string) {
    setPromoteTarget({ name, version });
    setPromoteFrom('');
    setPromoteTo('');
    setPromoteMsg(null);
  }

  async function handlePromote() {
    const target = promoteTarget();
    if (!target) return;
    setPromoteLoading(true);
    setPromoteMsg(null);
    try {
      const result = await promotePackage(target.name, target.version, promoteFrom(), promoteTo());
      setPromoteMsg(result.message || 'Promoted successfully');
    } catch (e: any) {
      setPromoteMsg(e.message || 'Promotion failed');
    }
    setPromoteLoading(false);
  }

  return (
    <>
      {/* Header -- matches Stitch admin-packages page */}
      <header style={{ "margin-bottom": '2rem' }}>
        <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '0.5rem' }}>
          <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-headline)', "font-weight": '500', color: 'var(--clr-primary)', "letter-spacing": '0.3em', "text-transform": 'uppercase' }}>Registry / Admin</span>
        </div>
        <h2 style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', color: 'var(--clr-on-surface)', "margin-bottom": '0' }}>Package Management</h2>
      </header>

      {/* Search Bar -- matches Stitch admin-packages */}
      <section style={{ "margin-bottom": '2rem' }}>
        <div style={{ position: 'relative', background: '#000', border: '1px solid rgba(67, 72, 78, 0.2)', "border-radius": '0.75rem', overflow: 'hidden' }}>
          <div style={{ position: 'absolute', left: '1rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--clr-primary)' }}>
            <span class="material-symbols-outlined" style={{ "font-size": '20px' }}>terminal</span>
          </div>
          <input
            type="text"
            value={inputValue()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            placeholder="Query Registry..."
            style={{ width: '100%', background: 'transparent', border: 'none', color: 'var(--clr-on-surface)', padding: '1rem 1rem 1rem 3rem', "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "font-size": '0.875rem', outline: 'none' }}
          />
        </div>
      </section>

      <Show when={data.loading}><LoadingSpinner /></Show>
      <Show when={data.error}><div class="alert alert-error">Failed to load packages.</div></Show>

      <Show when={data()}>
        {(d) => (
          <>
            <Show when={d().packages.length > 0} fallback={<EmptyState title="No packages" text="No packages found." />}>
              {/* Table layout -- matches Stitch admin-packages-full.html */}
              <div class="data-table-wrapper">
                <div class="table-header-bar">
                  <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
                    <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem', padding: '0.25rem 0.75rem', background: 'var(--clr-surface-container-lowest)', "border-radius": '0.375rem', border: '1px solid rgba(255, 255, 255, 0.05)' }}>
                      <span class="material-symbols-outlined" style={{ "font-size": '12px', color: 'var(--clr-primary)' }}>filter_list</span>
                      <span style={{ "font-size": '0.625rem', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)' }}>Filter By Format</span>
                    </div>
                  </div>
                  <div style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface-variant)', "letter-spacing": '-0.025em', "text-transform": 'uppercase' }}>
                    {d().total} results
                  </div>
                </div>
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Package</th>
                      <th>Repository</th>
                      <th style={{ "text-align": 'center' }}>Format</th>
                      <th style={{ "text-align": 'center' }}>Versions</th>
                      <th style={{ "text-align": 'center' }}>Security</th>
                      <th style={{ "text-align": 'right' }}>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={d().packages}>
                      {(p) => {
                        const fmt = guessFormat(p.name);
                        return (
                          <tr class="admin-pkg-row">
                            <td>
                              <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                                <div style={{ width: '32px', height: '32px', "border-radius": '0.25rem', background: 'var(--clr-surface-container-highest)', display: 'flex', "align-items": 'center', "justify-content": 'center' }}>
                                  <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)', "font-size": '14px' }}>package_2</span>
                                </div>
                                <div>
                                  <div style={{ "font-size": '0.875rem', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{p.name}</div>
                                  <div style={{ "font-size": '0.625rem', color: 'var(--clr-on-surface-variant)', "font-family": 'var(--font-mono)' }}>
                                    {p.latest_version} &bull; {p.downloads.toLocaleString()} DLS
                                  </div>
                                </div>
                              </div>
                            </td>
                            <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.6875rem', color: 'var(--clr-on-surface-variant)' }}>
                              --
                            </td>
                            <td style={{ "text-align": 'center' }}>
                              <span class={`format-badge ${formatBadgeClass(fmt)}`}>{fmt}</span>
                            </td>
                            <td style={{ "text-align": 'center', "font-family": 'var(--font-mono)', "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>
                              --
                            </td>
                            <td style={{ "text-align": 'center' }}>
                              {/* Vuln badge -- matches Stitch: green verified_user or red report */}
                              <span class="material-symbols-outlined vuln-icon-clean" style={{ "font-variation-settings": "'FILL' 1" }}>verified_user</span>
                            </td>
                            <td style={{ "text-align": 'right' }}>
                              <div class="admin-pkg-actions">
                                <button
                                  class="promote-btn"
                                  onClick={() => openPromote(p.name, p.latest_version)}
                                >
                                  Promote
                                </button>
                                <A href={`/packages/${p.name}`} style={{ "font-size": '0.5625rem', "font-family": 'var(--font-headline)', color: 'var(--clr-primary-dim)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "text-decoration": 'none' }}>
                                  View
                                </A>
                              </div>
                            </td>
                          </tr>
                        );
                      }}
                    </For>
                  </tbody>
                </table>

                {/* Pagination */}
                <div class="pagination">
                  <span class="pagination-info">Page {d().page} of {Math.ceil(d().total / d().page_size) || 1}</span>
                  <div style={{ display: 'flex', "align-items": 'center', gap: '0.25rem' }}>
                    <Show when={d().page > 1}>
                      <button class="pagination-btn" onClick={() => goToPage(d().page - 1)}>Previous</button>
                    </Show>
                    <Show when={d().has_next}>
                      <button class="pagination-btn" onClick={() => goToPage(d().page + 1)}>Next</button>
                    </Show>
                  </div>
                </div>
              </div>
            </Show>
          </>
        )}
      </Show>

      {/* Promote Modal */}
      <Show when={promoteTarget()}>
        {(target) => (
          <div class="modal-overlay" onClick={() => setPromoteTarget(null)}>
            <div class="modal" onClick={(e) => e.stopPropagation()}>
              <h3 class="modal-title">Promote Package</h3>
              <div class="modal-body">
                <p style={{ "margin-bottom": '1rem' }}>
                  Promote <strong style={{ color: 'var(--clr-primary)' }}>{target().name}@{target().version}</strong> between repositories.
                </p>
                <div class="form-group">
                  <label class="form-label">Source Repository</label>
                  <select
                    class="form-select"
                    value={promoteFrom()}
                    onChange={(e) => setPromoteFrom(e.currentTarget.value)}
                  >
                    <option value="">Select source...</option>
                    <Show when={repos()}>
                      <For each={repos()!.repositories}>
                        {(r) => <option value={r.name}>{r.name}</option>}
                      </For>
                    </Show>
                  </select>
                </div>
                <div class="form-group">
                  <label class="form-label">Target Repository</label>
                  <select
                    class="form-select"
                    value={promoteTo()}
                    onChange={(e) => setPromoteTo(e.currentTarget.value)}
                  >
                    <option value="">Select target...</option>
                    <Show when={repos()}>
                      <For each={repos()!.repositories}>
                        {(r) => <option value={r.name}>{r.name}</option>}
                      </For>
                    </Show>
                  </select>
                </div>
                <Show when={promoteMsg()}>
                  <div class="alert alert-info" style={{ "margin-top": '0.5rem' }}>{promoteMsg()}</div>
                </Show>
              </div>
              <div class="modal-actions">
                <button class="btn btn-secondary" onClick={() => setPromoteTarget(null)}>Cancel</button>
                <button
                  class="btn btn-primary"
                  onClick={handlePromote}
                  disabled={promoteLoading() || !promoteFrom() || !promoteTo()}
                >
                  {promoteLoading() ? 'Promoting...' : 'Promote'}
                </button>
              </div>
            </div>
          </div>
        )}
      </Show>
    </>
  );
}
