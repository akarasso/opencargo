import { For, Show, createResource, createSignal } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { FormatTag, LoadError, TableSkeleton } from '../components/bits.tsx';
import { fetchPackages, fetchRepositories } from '../core/api.ts';
import { useLive } from '../core/stores/live.ts';
import { formatNumber, timeAgo } from '../core/format.ts';

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
  const [data, { refetch }] = createResource(
    () => ({ q: query(), repo: repoFilter(), page: page() }),
    fetchPackages,
  );
  useLive(refetch, ['package.published', 'package.promoted', 'registry.changed']);

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  function handleInput(value: string) {
    setInputValue(value);
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      setSearchParams({ q: value || undefined, page: '1' });
    }, 280);
  }

  const totalPages = () => {
    const d = data();
    return d ? Math.max(1, Math.ceil(d.total / d.page_size)) : 1;
  };

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Packages</h1>
          <p class="page-sub">Everything published to or cached by this registry.</p>
        </div>
      </div>

      <div class="filter-bar">
        <div class="search-box">
          <Icon name="search" size={15} />
          <input
            class="input"
            type="text"
            value={inputValue()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            placeholder="Filter by package name…"
            spellcheck={false}
          />
        </div>
        <Show when={(repos()?.repositories.length ?? 0) > 0}>
          <select
            class="select"
            value={repoFilter()}
            onChange={(e) => setSearchParams({ repo: e.currentTarget.value || undefined, page: '1' })}
          >
            <option value="">All repositories</option>
            <For each={repos()?.repositories}>
              {(repo) => <option value={repo.name}>{repo.name}</option>}
            </For>
          </select>
        </Show>
      </div>

      <Show when={data.error}>
        <LoadError what="packages" />
      </Show>

      <Show when={data()} fallback={<TableSkeleton rows={8} cols={5} />}>
        {(d) => (
          <Show
            when={d().packages.length > 0}
            fallback={
              <div class="card">
                <EmptyState
                  icon="package"
                  title={query() ? 'No matches' : 'No packages yet'}
                  text={
                    query()
                      ? `Nothing matches “${query()}”${repoFilter() ? ` in ${repoFilter()}` : ''}.`
                      : 'Publish a package or install through a proxy repository and it will show up here.'
                  }
                />
              </div>
            }
          >
            <div class="table-card">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>Package</th>
                      <th>Latest</th>
                      <th class="cell-hide-sm">Description</th>
                      <th style={{ 'text-align': 'right' }}>Downloads</th>
                      <th style={{ 'text-align': 'right' }}>Updated</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={d().packages}>
                      {(p) => (
                        <tr>
                          <td>
                            <A class="row-link" href={`/packages/${p.name}`}>
                              {p.name}
                            </A>
                          </td>
                          <td>
                            <span class="version">{p.latest_version}</span>
                          </td>
                          <td class="cell-muted cell-hide-sm truncate" style={{ 'max-width': '340px' }}>
                            {p.description || '—'}
                          </td>
                          <td class="cell-mono cell-num" style={{ 'text-align': 'right' }}>
                            {formatNumber(p.downloads)}
                          </td>
                          <td class="cell-dim nowrap" style={{ 'text-align': 'right' }} title={p.published_at}>
                            {timeAgo(p.published_at)}
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
              <div class="pagination">
                <span class="pagination-info">
                  Page {d().page} / {totalPages()} · {formatNumber(d().total)} packages
                </span>
                <div class="pagination-nav">
                  <button
                    class="btn btn-ghost btn-sm"
                    disabled={d().page <= 1}
                    onClick={() => setSearchParams({ page: String(d().page - 1) })}
                  >
                    <Icon name="chevron-left" size={14} />
                    Prev
                  </button>
                  <button
                    class="btn btn-ghost btn-sm"
                    disabled={!d().has_next}
                    onClick={() => setSearchParams({ page: String(d().page + 1) })}
                  >
                    Next
                    <Icon name="chevron-right" size={14} />
                  </button>
                </div>
              </div>
            </div>
          </Show>
        )}
      </Show>
    </div>
  );
}
