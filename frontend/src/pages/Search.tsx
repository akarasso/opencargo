import { For, Show, createResource, createSignal } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { LoadError } from '../components/bits.tsx';
import { fetchSearch } from '../core/api.ts';

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
    }, 260);
  }

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Search</h1>
          <p class="page-sub">Full-text search across package names and descriptions.</p>
        </div>
      </div>

      <div class="search-box" style={{ 'margin-bottom': '18px' }}>
        <Icon name="search" size={16} />
        <input
          class="input"
          style={{ padding: '12px 40px 12px 38px', 'font-size': '0.95rem' }}
          type="text"
          value={inputValue()}
          onInput={(e) => handleInput(e.currentTarget.value)}
          placeholder="Search packages…"
          spellcheck={false}
          autofocus
        />
        <Show when={data.loading}>
          <span class="spinner" style={{ position: 'absolute', right: '12px' }} />
        </Show>
      </div>

      <Show when={data.error}>
        <LoadError what="search results" />
      </Show>

      <Show
        when={query()}
        fallback={
          <div class="card">
            <EmptyState
              icon="search"
              title="Type to search"
              text="Search covers every package you're allowed to see — private repositories stay private."
            />
          </div>
        }
      >
        <Show when={data()}>
          {(d) => (
            <Show
              when={d().results.length > 0}
              fallback={
                <div class="card">
                  <EmptyState
                    icon="search"
                    title="No results"
                    text={`Nothing matches “${d().query}”. Try a shorter or different term.`}
                  />
                </div>
              }
            >
              <p class="dim small" style={{ 'margin-bottom': '10px' }}>
                {d().results.length} result{d().results.length === 1 ? '' : 's'} for “{d().query}”
              </p>
              <div class="col stagger" style={{ gap: '10px' }}>
                <For each={d().results}>
                  {(r) => (
                    <A href={`/packages/${r.name}`} class="card card-pad card-hover" style={{ display: 'block' }}>
                      <div class="row">
                        <Icon name="package" size={15} class="icon dim" />
                        <span class="mono grow truncate" style={{ color: 'var(--ink)', 'font-weight': 500 }}>
                          {r.name}
                        </span>
                        <span class="version">{r.latest_version}</span>
                      </div>
                      <Show when={r.description}>
                        <p class="muted small truncate" style={{ 'margin-top': '6px' }}>
                          {r.description}
                        </p>
                      </Show>
                    </A>
                  )}
                </For>
              </div>
            </Show>
          )}
        </Show>
      </Show>
    </div>
  );
}
