import { createSignal, createResource, For, Show } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import { fetchPackages, fetchRepositories } from '../lib/api.ts';
import LoadingSpinner from '../components/LoadingSpinner.tsx';
import EmptyState from '../components/EmptyState.tsx';

function paramStr(val: string | string[] | undefined): string {
  if (Array.isArray(val)) return val[0] ?? '';
  return val ?? '';
}

export default function Packages() {
  const [searchParams, setSearchParams] = useSearchParams();

  const query = () => paramStr(searchParams.q);
  const repoFilter = () => paramStr(searchParams.repo);
  const page = () => parseInt(paramStr(searchParams.page) || '1', 10) || 1;

  const [inputValue, setInputValue] = createSignal(query());

  const [repos] = createResource(fetchRepositories);
  const [data] = createResource(
    () => ({ q: query(), repo: repoFilter(), page: page() }),
    fetchPackages,
  );

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

  return (
    <>
      {/* Header -- matches Stitch packages page */}
      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center', "margin-bottom": '2rem' }}>
        <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.25rem' }}>
          <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem' }}>
            <div style={{ width: '6px', height: '6px', background: 'var(--clr-primary)', "border-radius": '50%' }} class="status-led-animated" />
            <span style={{ "font-family": "var(--font-label)", "font-size": "0.625rem", "text-transform": "uppercase", "letter-spacing": "0.2em", color: "var(--clr-primary)", "font-weight": "700" }}>Live Registry Stream</span>
          </div>
          <h1 style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', color: 'var(--clr-on-surface)' }}>Package Registry</h1>
        </div>
      </div>

      {/* Filters Bar -- matches Stitch */}
      <div style={{ display: 'flex', gap: '1rem', "margin-bottom": '2rem' }}>
        <div style={{ flex: '1', position: 'relative' }}>
          <span class="material-symbols-outlined" style={{ position: 'absolute', left: '1rem', top: '50%', transform: 'translateY(-50%)', color: 'rgb(100, 116, 139)' }}>search</span>
          <input
            style={{ width: '100%', background: 'var(--clr-surface-container)', border: 'none', "border-radius": '0.5rem', padding: '1rem 1rem 1rem 3rem', color: 'var(--clr-on-surface)', "font-family": 'var(--font-body)', "font-size": '0.875rem', outline: 'none' }}
            type="text"
            value={inputValue()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            placeholder="Filter packages by name or hash..."
            onFocus={(e) => { e.currentTarget.style.boxShadow = '0 0 0 2px rgba(123, 231, 249, 0.2)'; }}
            onBlur={(e) => { e.currentTarget.style.boxShadow = 'none'; }}
          />
        </div>
        <Show when={repos()}>
          {(r) => (
            <Show when={r().repositories.length > 0}>
              <div style={{ width: '256px', position: 'relative' }}>
                <select
                  class="filter-select"
                  style={{ width: '100%' }}
                  value={repoFilter()}
                  onChange={(e) => handleRepoChange(e.currentTarget.value)}
                >
                  <option value="">Repo: All</option>
                  <For each={r().repositories}>
                    {(repo) => <option value={repo.name}>Repo: {repo.name}</option>}
                  </For>
                </select>
                <span class="material-symbols-outlined" style={{ position: 'absolute', right: '1rem', top: '50%', transform: 'translateY(-50%)', color: 'rgb(100, 116, 139)', "pointer-events": 'none' }}>expand_more</span>
              </div>
            </Show>
          )}
        </Show>
      </div>

      <Show when={data.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={data.error}>
        <div class="alert alert-error">Failed to load packages.</div>
      </Show>

      <Show when={data()}>
        {(d) => (
          <>
            <Show
              when={d().packages.length > 0}
              fallback={
                <EmptyState
                  title="No packages found"
                  text={query() ? `No packages match "${query()}".` : 'No packages have been published yet.'}
                />
              }
            >
              {/* Registry Table -- matches Stitch */}
              <div class="data-table-wrapper">
                <div style={{ "overflow-x": 'auto' }}>
                  <table class="data-table">
                    <thead>
                      <tr>
                        <th>Package Entity</th>
                        <th>Version Hash</th>
                        <th>Description Metadata</th>
                        <th>Sync Load</th>
                        <th>Committed</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={d().packages}>
                        {(p) => (
                          <tr>
                            <td>
                              <A href={`/packages/${p.name}`} style={{ "font-family": 'var(--font-mono)', "font-size": '0.875rem', color: 'var(--clr-primary)', "text-decoration": 'none' }}>
                                {p.name}
                              </A>
                            </td>
                            <td>
                              <span style={{ background: 'rgba(123, 231, 249, 0.1)', color: 'var(--clr-primary)', padding: '0.125rem 0.5rem', "border-radius": 'var(--radius-sm)', "font-size": '0.625rem', "font-weight": '700', "letter-spacing": '0.05em', "font-family": 'var(--font-headline)' }}>{p.latest_version}</span>
                            </td>
                            <td style={{ "font-size": '0.875rem', color: 'rgb(148, 163, 184)' }}>{p.description || '--'}</td>
                            <td style={{ "font-size": '0.875rem', "font-family": 'var(--font-mono)', color: 'var(--clr-secondary-fixed-dim)' }}>{p.downloads.toLocaleString()}</td>
                            <td style={{ "font-size": '0.75rem', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>{p.published_at}</td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                </div>
              </div>

              {/* Pagination -- matches Stitch */}
              <div style={{ "margin-top": '2rem', background: 'var(--clr-surface-container-low)', padding: '1rem', "border-radius": '0.5rem', border: '1px solid rgba(255, 255, 255, 0.05)', display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
                  <p style={{ "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "font-size": '0.625rem', color: 'rgb(100, 116, 139)', "margin-bottom": '0' }}>Page {d().page} of {Math.ceil(d().total / d().page_size) || 1}</p>
                  <div style={{ height: '4px', width: '96px', background: 'rgba(255, 255, 255, 0.05)', "border-radius": '9999px', overflow: 'hidden' }}>
                    <div style={{ height: '100%', background: 'var(--clr-primary)', width: `${Math.min(100, (d().page / (Math.ceil(d().total / d().page_size) || 1)) * 100)}%` }} />
                  </div>
                </div>
                <div style={{ display: 'flex', gap: '0.5rem' }}>
                  <Show when={d().page > 1}>
                    <button
                      onClick={() => goToPage(d().page - 1)}
                      style={{ width: '40px', height: '40px', display: 'flex', "align-items": 'center', "justify-content": 'center', "border-radius": 'var(--radius-sm)', background: 'var(--clr-surface-container-high)', border: '1px solid rgba(255, 255, 255, 0.05)', color: 'var(--clr-on-surface)', cursor: 'pointer' }}
                    >
                      <span class="material-symbols-outlined">chevron_left</span>
                    </button>
                  </Show>
                  <Show when={d().has_next}>
                    <button
                      onClick={() => goToPage(d().page + 1)}
                      style={{ width: '40px', height: '40px', display: 'flex', "align-items": 'center', "justify-content": 'center', "border-radius": 'var(--radius-sm)', background: 'var(--clr-surface-container-high)', border: '1px solid rgba(255, 255, 255, 0.05)', color: 'var(--clr-on-surface)', cursor: 'pointer' }}
                    >
                      <span class="material-symbols-outlined">chevron_right</span>
                    </button>
                  </Show>
                </div>
              </div>
            </Show>
          </>
        )}
      </Show>
    </>
  );
}
