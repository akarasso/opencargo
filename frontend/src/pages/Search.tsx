import { For, Show, createResource, createSignal } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { LoadError } from '../components/bits.tsx';
import { fetchSearch } from '../core/api.ts';
import { createDebounced } from '../core/debounce.ts';
import { timeAgo } from '../core/format.ts';
import { paramStr } from '../core/params.ts';
import type { SearchResult } from '../core/types.ts';

// A proxied hit has no package page to open: nothing was published here, so
// the card names the repository that serves it instead of linking nowhere.
function Result(props: { hit: SearchResult }) {
  const body = () => (
    <>
      <div class="row">
        <Icon name="package" size={15} class="icon dim" />
        <span class="mono grow truncate" style={{ color: 'var(--ink)', 'font-weight': 500 }}>
          {props.hit.name}
        </span>
        <Show when={props.hit.source === 'cached'}>
          <span class="chip chip-info" title={`Served through ${props.hit.repository}`}>
            proxied · {props.hit.repository}
          </span>
        </Show>
        <span class="version">{props.hit.latest_version}</span>
      </div>
      <Show when={props.hit.description}>
        <p class="muted small truncate" style={{ 'margin-top': '6px' }}>
          {props.hit.description}
        </p>
      </Show>
      <Show when={props.hit.source === 'cached' && props.hit.last_seen}>
        <p class="dim small" style={{ 'margin-top': '6px' }}>
          Last served {timeAgo(props.hit.last_seen)} — install it from{' '}
          <span class="mono">{props.hit.repository}</span>
        </p>
      </Show>
    </>
  );

  return (
    <Show
      when={props.hit.source === 'hosted'}
      fallback={<div class="card card-pad">{body()}</div>}
    >
      <A href={`/packages/${props.hit.name}`} class="card card-pad card-hover" style={{ display: 'block' }}>
        {body()}
      </A>
    </Show>
  );
}

export default function Search() {
  const [searchParams, setSearchParams] = useSearchParams();
  const query = () => paramStr(searchParams.q);
  const [inputValue, setInputValue] = createSignal(query());

  const [data] = createResource(query, fetchSearch);

  const debouncedSearch = createDebounced((value: string) => {
    setSearchParams({ q: value || undefined });
  }, 260);
  function handleInput(value: string) {
    setInputValue(value);
    debouncedSearch(value);
  }

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Search</h1>
          <p class="page-sub">
            Full-text search across package names and descriptions, hosted and proxied.
          </p>
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
              text="Search covers every package you're allowed to see, including the ones a proxy repository has already served — private repositories stay private."
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
                <For each={d().results}>{(r) => <Result hit={r} />}</For>
              </div>
            </Show>
          )}
        </Show>
      </Show>
    </div>
  );
}
