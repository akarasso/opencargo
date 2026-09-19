import { For, Show, createMemo, createResource, createSignal } from 'solid-js';
import { useSearchParams } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import CopyButton from '../components/CopyButton.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { LoadError, TableSkeleton } from '../components/bits.tsx';
import { fetchRawFiles, fetchRepositories } from '../core/api.ts';
import { createDebounced } from '../core/debounce.ts';
import { formatBytes, formatNumber, timeAgo } from '../core/format.ts';
import { paramStr } from '../core/params.ts';
import { useLive } from '../core/stores/live.ts';

/**
 * The content of a raw repository: arbitrary paths, their size and their
 * sha256. A group lists its hosted members merged, the first member holding
 * a path winning it.
 */
export default function RawFiles() {
  const [searchParams, setSearchParams] = useSearchParams();
  const [repos] = createResource(fetchRepositories);

  const rawRepos = createMemo(() =>
    (repos()?.repositories ?? []).filter((r) => r.format === 'raw'),
  );
  const repo = () => paramStr(searchParams.repo) || rawRepos()[0]?.name || '';
  const prefix = () => paramStr(searchParams.prefix);
  const page = () => parseInt(paramStr(searchParams.page) || '1', 10) || 1;

  const [inputValue, setInputValue] = createSignal(prefix());
  const debounced = createDebounced((value: string) => {
    setSearchParams({ prefix: value || undefined, page: '1' });
  }, 280);

  const [data, { refetch }] = createResource(
    () => (repo() ? { repo: repo(), prefix: prefix(), page: page() } : undefined),
    fetchRawFiles,
  );
  useLive(refetch, ['package.published', 'registry.changed']);

  const base = () => `${location.protocol}//${location.host}`;
  /** A stored path may hold `#`, `?` or a space: each segment is a URL segment. */
  const fileUrl = (path: string) =>
    `${base()}/raw/${repo()}/${path.split('/').map(encodeURIComponent).join('/')}`;
  const totalPages = () => {
    const d = data();
    return d ? Math.max(1, Math.ceil(d.total / d.pageSize)) : 1;
  };

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Raw files</h1>
          <p class="page-sub">
            Any file at any path: build artefacts, toolchains, archives — served over plain HTTP.
          </p>
        </div>
      </div>

      <Show
        when={rawRepos().length > 0}
        fallback={
          <div class="card">
            <EmptyState
              icon="layers"
              title="No raw repository yet"
              text="Create a repository with format “raw” to store arbitrary files."
            />
          </div>
        }
      >
        <div class="filter-bar">
          <select
            class="select"
            value={repo()}
            onChange={(e) => setSearchParams({ repo: e.currentTarget.value, page: '1' })}
          >
            <For each={rawRepos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
          </select>
          <div class="search-box">
            <Icon name="search" size={15} />
            <input
              class="input"
              type="text"
              value={inputValue()}
              onInput={(e) => {
                setInputValue(e.currentTarget.value);
                debounced(e.currentTarget.value);
              }}
              placeholder="Filter by path prefix, e.g. dist/linux-amd64"
              spellcheck={false}
            />
          </div>
        </div>

        <Show when={data.error}>
          <LoadError what="raw files" />
        </Show>

        <Show when={data()} fallback={<TableSkeleton rows={8} cols={5} />}>
          {(d) => (
            <Show
              when={d().files.length > 0}
              fallback={
                <div class="card">
                  <EmptyState
                    icon="layers"
                    title={prefix() ? 'No matches' : 'Nothing stored yet'}
                    text={
                      prefix()
                        ? `No path under “${prefix()}” in ${repo()}.`
                        : `Upload a file with: curl -T ./tool.tar.gz ${base()}/raw/${repo()}/dist/tool.tar.gz`
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
                        <th>Path</th>
                        <th class="cell-hide-sm">Repository</th>
                        <th style={{ 'text-align': 'right' }}>Size</th>
                        <th class="cell-hide-sm">sha256</th>
                        <th class="cell-hide-sm">By</th>
                        <th style={{ 'text-align': 'right' }}>Uploaded</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={d().files}>
                        {(f) => (
                          <tr>
                            <td>
                              <a class="row-link" href={fileUrl(f.path)}>
                                {f.path}
                              </a>
                            </td>
                            <td class="cell-muted cell-hide-sm">{f.repository}</td>
                            <td class="cell-num nowrap" style={{ 'text-align': 'right' }}>
                              {formatBytes(f.size)}
                            </td>
                            <td class="cell-mono cell-hide-sm nowrap" title={f.sha256}>
                              {f.sha256.slice(0, 12)}…
                              <CopyButton text={f.sha256} label="" />
                            </td>
                            <td class="cell-muted cell-hide-sm">{f.uploadedBy}</td>
                            <td
                              class="cell-dim nowrap"
                              style={{ 'text-align': 'right' }}
                              title={f.uploadedAt}
                            >
                              {timeAgo(f.uploadedAt)}
                            </td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                </div>
                <div class="pagination">
                  <span class="pagination-info">
                    Page {d().page} / {totalPages()} · {formatNumber(d().total)} files
                    <Show when={d().truncated}> · listing truncated</Show>
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
                      disabled={!d().hasNext}
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

        <section class="section">
          <div class="section-head">
            <span class="section-title">Quickstart</span>
          </div>
          <div class="card card-pad col" style={{ gap: '14px' }}>
            <For
              each={[
                {
                  label: '1 · Upload',
                  command: `curl -u user:token -T ./tool.tar.gz ${base()}/raw/${repo()}/dist/tool.tar.gz`,
                },
                {
                  label: '2 · Download',
                  command: `curl -O ${base()}/raw/${repo()}/dist/tool.tar.gz`,
                },
                {
                  label: '3 · Remove',
                  command: `curl -u user:token -X DELETE ${base()}/raw/${repo()}/dist/tool.tar.gz`,
                },
              ]}
            >
              {(step) => (
                <div>
                  <div class="side-label">{step.label}</div>
                  <div class="code-line">
                    <code>{step.command}</code>
                    <CopyButton text={step.command} label="" />
                  </div>
                </div>
              )}
            </For>
          </div>
        </section>
      </Show>
    </div>
  );
}
