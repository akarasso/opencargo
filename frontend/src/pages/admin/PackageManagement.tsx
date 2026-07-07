import { For, Show, createResource, createSignal } from 'solid-js';
import { A, useSearchParams } from '@solidjs/router';
import Icon from '../../components/Icon.tsx';
import Modal from '../../components/Modal.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import { fetchPackages, fetchRepositories, promotePackage } from '../../core/api.ts';
import { useLive } from '../../core/stores/live.ts';
import { toasts } from '../../core/stores/toasts.ts';
import { formatNumber, timeAgo } from '../../core/format.ts';

function paramStr(val: string | string[] | undefined): string {
  if (Array.isArray(val)) return val[0] ?? '';
  return val ?? '';
}

export default function PackageManagement() {
  return (
    <RequireAdmin>
      <PackageManagementInner />
    </RequireAdmin>
  );
}

function PackageManagementInner() {
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

  const [promoteTarget, setPromoteTarget] = createSignal<{ name: string; version: string } | null>(null);
  const [promoteFrom, setPromoteFrom] = createSignal('');
  const [promoteTo, setPromoteTo] = createSignal('');
  const [promoteLoading, setPromoteLoading] = createSignal(false);

  const hostedRepos = () => (repos()?.repositories ?? []).filter((r) => r.type === 'hosted');

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  function handleInput(value: string) {
    setInputValue(value);
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      setSearchParams({ q: value || undefined, page: '1' });
    }, 280);
  }

  async function handlePromote() {
    const target = promoteTarget();
    if (!target) return;
    setPromoteLoading(true);
    try {
      await promotePackage(target.name, target.version, promoteFrom(), promoteTo());
      toasts.success(`${target.name} ${target.version} promoted`, `${promoteFrom()} → ${promoteTo()}`);
      setPromoteTarget(null);
      void refetch();
    } catch (e: unknown) {
      toasts.error('Promotion failed', e instanceof Error ? e.message : undefined);
    }
    setPromoteLoading(false);
  }

  const totalPages = () => {
    const d = data();
    return d ? Math.max(1, Math.ceil(d.total / d.page_size)) : 1;
  };

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Promotion</h1>
          <p class="page-sub">
            Move versions between hosted repositories (dev → prod). The tarball is shared, so
            lockfiles keep resolving.
          </p>
        </div>
      </div>

      <div class="filter-bar">
        <div class="search-box">
          <Icon name="search" size={15} />
          <input
            class="input"
            value={inputValue()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            placeholder="Filter packages…"
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
            <For each={repos()?.repositories}>{(r) => <option value={r.name}>{r.name}</option>}</For>
          </select>
        </Show>
      </div>

      <Show when={data.error}>
        <LoadError what="packages" />
      </Show>

      <Show when={data()} fallback={<TableSkeleton rows={8} cols={4} />}>
        {(d) => (
          <Show
            when={d().packages.length > 0}
            fallback={
              <div class="card">
                <EmptyState icon="layers" title="Nothing to promote" text="No packages match the current filter." />
              </div>
            }
          >
            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>Package</th>
                      <th>Latest</th>
                      <th style={{ 'text-align': 'right' }}>Downloads</th>
                      <th class="cell-hide-sm" style={{ 'text-align': 'right' }}>
                        Updated
                      </th>
                      <th style={{ 'text-align': 'right' }}>Actions</th>
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
                          <td class="cell-mono cell-num" style={{ 'text-align': 'right' }}>
                            {formatNumber(p.downloads)}
                          </td>
                          <td class="cell-dim cell-hide-sm nowrap" style={{ 'text-align': 'right' }} title={p.published_at}>
                            {timeAgo(p.published_at)}
                          </td>
                          <td>
                            <div class="cell-actions">
                              <button
                                class="btn btn-ghost btn-sm"
                                onClick={() => {
                                  setPromoteTarget({ name: p.name, version: p.latest_version });
                                  setPromoteFrom('');
                                  setPromoteTo('');
                                }}
                              >
                                <Icon name="arrow-up-right" size={13} />
                                Promote
                              </button>
                            </div>
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

      <Modal
        open={promoteTarget() !== null}
        title="Promote a version"
        subtitle={`${promoteTarget()?.name}@${promoteTarget()?.version}`}
        onClose={() => setPromoteTarget(null)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setPromoteTarget(null)}>
              Cancel
            </button>
            <button
              class="btn btn-primary"
              onClick={handlePromote}
              disabled={promoteLoading() || !promoteFrom() || !promoteTo() || promoteFrom() === promoteTo()}
            >
              {promoteLoading() ? 'Promoting…' : 'Promote'}
            </button>
          </>
        }
      >
        <div class="field">
          <label class="field-label">From repository</label>
          <select class="select" value={promoteFrom()} onChange={(e) => setPromoteFrom(e.currentTarget.value)}>
            <option value="">Select source…</option>
            <For each={hostedRepos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
          </select>
        </div>
        <div class="field">
          <label class="field-label">To repository</label>
          <select class="select" value={promoteTo()} onChange={(e) => setPromoteTo(e.currentTarget.value)}>
            <option value="">Select target…</option>
            <For each={hostedRepos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
          </select>
          <div class="field-hint">Both must be hosted repositories of the same format.</div>
        </div>
      </Modal>
    </div>
  );
}
