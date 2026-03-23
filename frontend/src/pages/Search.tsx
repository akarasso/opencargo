import { createSignal, createResource, For, Show } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import { fetchSearch } from '../lib/api.ts';
import LoadingSpinner from '../components/LoadingSpinner.tsx';
import EmptyState from '../components/EmptyState.tsx';

function paramStr(val: string | string[] | undefined): string {
  if (Array.isArray(val)) return val[0] ?? '';
  return val ?? '';
}

export default function Search() {
  const [searchParams, setSearchParams] = useSearchParams();
  const query = () => paramStr(searchParams.q);
  const [inputValue, setInputValue] = createSignal(query());

  const [data] = createResource(query, fetchSearch);

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;

  function handleInput(value: string) {
    setInputValue(value);
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      setSearchParams({ q: value || undefined });
    }, 300);
  }

  return (
    <div style={{ "max-width": '64rem', margin: '0 auto', padding: '4rem 2rem' }}>
      {/* Hero Section -- matches Stitch search page */}
      <section style={{ "text-align": 'center', "margin-bottom": '4rem' }}>
        <div style={{ display: 'inline-flex', "align-items": 'center', gap: '0.5rem', "margin-bottom": '1.5rem', padding: '0.25rem 0.75rem', background: 'rgba(123, 231, 249, 0.1)', "border-radius": '9999px', border: '1px solid rgba(123, 231, 249, 0.2)' }}>
          <span style={{ display: 'flex', height: '8px', width: '8px', "border-radius": '50%', background: 'var(--clr-primary)' }} class="status-led-animated" />
          <span style={{ "font-family": "var(--font-label)", "font-size": "0.625rem", "text-transform": "uppercase", "letter-spacing": "0.2em", color: "var(--clr-primary)", "font-weight": "700" }}>Registry Node: Live</span>
        </div>
        <h1 style={{ "font-family": 'var(--font-headline)', "font-size": '3.5rem', "font-weight": '700', color: 'var(--clr-on-background)', "margin-bottom": '1rem', "letter-spacing": '-0.05em' }}>
          Search Packages
        </h1>
        <p style={{ color: 'var(--clr-on-surface-variant)', "font-family": 'var(--font-body)', "font-size": '1.125rem', "max-width": '36rem', margin: '0 auto' }}>
          Access the distributed global registry with high-fidelity indexing and real-time dependency mapping.
        </p>
      </section>

      {/* Search Interface -- matches Stitch */}
      <section style={{ "margin-bottom": '3rem' }}>
        <div class="search-outer" style={{ position: 'relative' }}>
          <div class="search-glow" />
          <div class="search-bar-container">
            <div style={{ "padding-left": '1.5rem', "padding-right": '1rem', color: 'var(--clr-primary)' }}>
              <span class="material-symbols-outlined" style={{ "font-size": '24px' }}>search</span>
            </div>
            <input
              type="text"
              value={inputValue()}
              onInput={(e) => handleInput(e.currentTarget.value)}
              placeholder="Search packages by name or description"
              autofocus
              style={{ width: '100%', background: 'transparent', border: 'none', padding: '1.5rem 0', color: 'var(--clr-on-background)', "font-family": 'var(--font-body)', "font-size": '1.125rem', outline: 'none' }}
            />
            <div style={{ "padding-right": '1.5rem', display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
              <kbd style={{ display: 'flex', "align-items": 'center', gap: '0.25rem', padding: '0.25rem 0.5rem', background: 'var(--clr-surface-container-high)', border: '1px solid rgba(67, 72, 78, 0.5)', "border-radius": 'var(--radius-sm)', "font-size": '0.625rem', color: 'rgb(148, 163, 184)', "font-family": 'var(--font-headline)', "font-weight": '700' }}>
                <span>CTRL</span>
                <span>K</span>
              </kbd>
              <button class="btn-primary" style={{ padding: '0.5rem 1.5rem', "border-radius": '0.5rem', border: 'none', background: 'var(--gradient-kinetic)', color: 'var(--clr-on-primary-container)', "font-family": 'var(--font-headline)', "font-weight": '700', "font-size": '0.75rem', "letter-spacing": '0.2em', "text-transform": 'uppercase', cursor: 'pointer' }}>
                Execute
              </button>
            </div>
          </div>
        </div>
      </section>

      <Show when={data.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={data.error}>
        <div class="alert alert-error">Search failed.</div>
      </Show>

      {/* Search Results -- matches Stitch result cards */}
      <Show when={query()}>
        <Show when={data()}>
          {(d) => (
            <Show
              when={d().results.length > 0}
              fallback={
                <EmptyState
                  title="No results"
                  text={`No packages found for "${query()}".`}
                />
              }
            >
              <section>
                <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1.5rem', padding: '0 0.5rem' }}>
                  <h2 style={{ "font-family": 'var(--font-headline)', "font-size": '0.75rem', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(148, 163, 184)', "margin-bottom": '0' }}>
                    Registry Matches ({d().results.length})
                  </h2>
                  <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
                    <span style={{ "font-family": 'var(--font-headline)', "font-size": '0.625rem', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(100, 116, 139)' }}>Sort by: Relevance</span>
                    <span class="material-symbols-outlined" style={{ color: 'rgb(100, 116, 139)', "font-size": '14px' }}>filter_list</span>
                  </div>
                </div>

                <div class="search-result-list">
                  <For each={d().results}>
                    {(r) => (
                      <A
                        href={`/packages/${r.name}`}
                        class="search-result-card"
                        style={{ "text-decoration": 'none', color: 'inherit', display: 'block' }}
                      >
                        <div class="search-result-card-inner">
                          <div style={{ flex: '1' }}>
                            <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem', "margin-bottom": '0.5rem' }}>
                              <span style={{ color: 'var(--clr-primary)', "font-family": 'var(--font-mono)', "font-size": '1.125rem', "font-weight": '500' }}>{r.name}</span>
                              <span style={{ padding: '0.125rem 0.5rem', background: 'var(--clr-surface-container-highest)', border: '1px solid rgba(67, 72, 78, 0.5)', "border-radius": 'var(--radius-sm)', "font-size": '0.625rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-background)', "letter-spacing": '0.05em' }}>{r.latest_version}</span>
                            </div>
                            <p style={{ color: 'var(--clr-on-surface-variant)', "font-family": 'var(--font-body)', "font-size": '0.875rem', "line-height": '1.6', "max-width": '42rem' }}>
                              {r.description || 'No description available.'}
                            </p>
                          </div>
                          <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
                            <div style={{ display: 'flex', "flex-direction": 'column', "align-items": 'flex-end', gap: '0.25rem' }}>
                              <span style={{ padding: '0.25rem 0.75rem', background: 'rgba(123, 231, 249, 0.1)', color: 'var(--clr-primary)', border: '1px solid rgba(123, 231, 249, 0.2)', "border-radius": '9999px', "font-size": '0.625rem', "font-family": 'var(--font-headline)', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>npm</span>
                            </div>
                            <span class="material-symbols-outlined" style={{ color: 'rgb(148, 163, 184)' }}>chevron_right</span>
                          </div>
                        </div>
                      </A>
                    )}
                  </For>
                </div>
              </section>
            </Show>
          )}
        </Show>
      </Show>

      <Show when={!query()}>
        <EmptyState
          title="Start searching"
          text="Type a package name or keyword to search."
        />
      </Show>
    </div>
  );
}
