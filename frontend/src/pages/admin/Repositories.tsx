import { For, Show, createSignal } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import Modal, { ConfirmModal } from '../../components/Modal.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { FormatTag, LoadError, RepoTypeChip, TableSkeleton, VisibilityChip } from '../../components/bits.tsx';
import {
  createRepository,
  deleteRepository,
  fetchRepositories,
  purgeRepositoryCache,
  updateRepository,
} from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';
import { toasts } from '../../core/stores/toasts.ts';
import type { Repository } from '../../core/types.ts';

export default function Repositories() {
  return (
    <RequireAdmin>
      <RepositoriesInner />
    </RequireAdmin>
  );
}

function RepositoriesInner() {
  const [repos, refetch] = createLiveResource(fetchRepositories, ['repositories.changed']);

  const [showCreate, setShowCreate] = createSignal(false);
  const [editing, setEditing] = createSignal<Repository | null>(null);
  const [deleting, setDeleting] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  // Create form
  const [name, setName] = createSignal('');
  const [type, setType] = createSignal('hosted');
  const [format, setFormat] = createSignal('npm');
  const [visibility, setVisibility] = createSignal('private');
  const [upstream, setUpstream] = createSignal('');
  const [members, setMembers] = createSignal('');

  // Edit form
  const [editVisibility, setEditVisibility] = createSignal('private');
  const [editUpstream, setEditUpstream] = createSignal('');
  const [editMembers, setEditMembers] = createSignal('');

  async function handleCreate(e: Event) {
    e.preventDefault();
    if (!name()) return;
    setBusy(true);
    try {
      await createRepository({
        name: name(),
        type: type(),
        format: format(),
        visibility: visibility(),
        upstream: type() === 'proxy' ? upstream() || undefined : undefined,
        members:
          type() === 'group'
            ? members()
                .split(',')
                .map((m) => m.trim())
                .filter(Boolean)
            : undefined,
      });
      toasts.success(`Repository ${name()} created`);
      setShowCreate(false);
      setName('');
      setUpstream('');
      setMembers('');
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not create repository', err instanceof Error ? err.message : undefined);
    }
    setBusy(false);
  }

  function openEdit(repo: Repository) {
    setEditing(repo);
    setEditVisibility(repo.visibility);
    setEditUpstream(repo.upstream ?? '');
    setEditMembers('');
  }

  async function handleEdit(e: Event) {
    e.preventDefault();
    const repo = editing();
    if (!repo) return;
    setBusy(true);
    try {
      await updateRepository(repo.name, {
        visibility: editVisibility(),
        upstream: repo.type === 'proxy' ? editUpstream() || undefined : undefined,
        members:
          repo.type === 'group' && editMembers()
            ? editMembers()
                .split(',')
                .map((m) => m.trim())
                .filter(Boolean)
            : undefined,
      });
      toasts.success(`Repository ${repo.name} updated`);
      setEditing(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not update repository', err instanceof Error ? err.message : undefined);
    }
    setBusy(false);
  }

  async function handleDelete() {
    const target = deleting();
    if (!target) return;
    try {
      await deleteRepository(target);
      toasts.success(`Repository ${target} deleted`);
      setDeleting(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not delete repository', err instanceof Error ? err.message : undefined);
      setDeleting(null);
    }
  }

  async function handlePurge(repoName: string) {
    try {
      const res = await purgeRepositoryCache(repoName);
      toasts.success('Cache purged', res.message);
    } catch (err: unknown) {
      toasts.error('Could not purge cache', err instanceof Error ? err.message : undefined);
    }
  }

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Repositories</h1>
          <p class="page-sub">
            Hosted repos store what you publish, proxies cache upstream registries, groups combine
            both behind one endpoint.
          </p>
        </div>
        <div class="page-actions">
          <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
            <Icon name="plus" size={14} />
            New repository
          </button>
        </div>
      </div>

      <Show when={repos.error}>
        <LoadError what="repositories" />
      </Show>

      <Show when={repos()} fallback={<TableSkeleton rows={5} cols={5} />}>
        {(r) => (
          <Show
            when={r().repositories.length > 0}
            fallback={
              <div class="card">
                <EmptyState
                  icon="database"
                  title="No repositories yet"
                  text="Create a hosted repository to publish into, or a proxy to cache an upstream registry."
                >
                  <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
                    <Icon name="plus" size={14} />
                    New repository
                  </button>
                </EmptyState>
              </div>
            }
          >
            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Type</th>
                      <th>Format</th>
                      <th>Visibility</th>
                      <th class="cell-hide-sm">Upstream</th>
                      <th style={{ 'text-align': 'right' }}>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={r().repositories}>
                      {(repo) => (
                        <tr>
                          <td>
                            <span class="mono" style={{ color: 'var(--ink)', 'font-weight': 500 }}>
                              {repo.name}
                            </span>
                          </td>
                          <td>
                            <RepoTypeChip type={repo.type} />
                          </td>
                          <td>
                            <FormatTag format={repo.format} />
                          </td>
                          <td>
                            <VisibilityChip visibility={repo.visibility} />
                          </td>
                          <td class="cell-dim cell-mono cell-hide-sm truncate" style={{ 'max-width': '220px' }}>
                            {repo.upstream || '—'}
                          </td>
                          <td>
                            <div class="cell-actions">
                              <Show when={repo.type === 'proxy'}>
                                <button
                                  class="btn btn-quiet btn-icon"
                                  title="Purge cached upstream artifacts"
                                  onClick={() => handlePurge(repo.name)}
                                >
                                  <Icon name="refresh" size={14} />
                                </button>
                              </Show>
                              <button class="btn btn-quiet btn-icon" title="Edit repository" onClick={() => openEdit(repo)}>
                                <Icon name="pencil" size={14} />
                              </button>
                              <button
                                class="btn btn-quiet btn-icon"
                                title="Delete repository (must be empty)"
                                onClick={() => setDeleting(repo.name)}
                              >
                                <Icon name="trash" size={14} />
                              </button>
                            </div>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </div>
          </Show>
        )}
      </Show>

      {/* Create */}
      <Modal
        open={showCreate()}
        title="New repository"
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleCreate} disabled={busy() || !name()}>
              {busy() ? 'Creating…' : 'Create repository'}
            </button>
          </>
        }
      >
        <form onSubmit={handleCreate}>
          <div class="field">
            <label class="field-label">Name</label>
            <input
              class="input"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              placeholder="npm-private"
              spellcheck={false}
              required
            />
          </div>
          <div class="form-row">
            <div class="field">
              <label class="field-label">Type</label>
              <select class="select" value={type()} onChange={(e) => setType(e.currentTarget.value)}>
                <option value="hosted">hosted — you publish into it</option>
                <option value="proxy">proxy — caches an upstream</option>
                <option value="group">group — one endpoint over several</option>
              </select>
            </div>
            <div class="field">
              <label class="field-label">Format</label>
              <select class="select" value={format()} onChange={(e) => setFormat(e.currentTarget.value)}>
                <option value="npm">npm</option>
                <option value="cargo">cargo</option>
                <option value="oci">oci</option>
                <option value="go">go</option>
              </select>
            </div>
          </div>
          <div class="field">
            <label class="field-label">Visibility</label>
            <select class="select" value={visibility()} onChange={(e) => setVisibility(e.currentTarget.value)}>
              <option value="private">private — read requires permission</option>
              <option value="public">public — anyone can read</option>
            </select>
          </div>
          <Show when={type() === 'proxy'}>
            <div class="field">
              <label class="field-label">Upstream URL</label>
              <input
                class="input"
                value={upstream()}
                onInput={(e) => setUpstream(e.currentTarget.value)}
                placeholder="https://registry.npmjs.org"
                spellcheck={false}
              />
            </div>
          </Show>
          <Show when={type() === 'group'}>
            <div class="field">
              <label class="field-label">Members (resolution order)</label>
              <input
                class="input"
                value={members()}
                onInput={(e) => setMembers(e.currentTarget.value)}
                placeholder="npm-private, npm-proxy"
                spellcheck={false}
              />
              <div class="field-hint">Comma-separated; the first match wins.</div>
            </div>
          </Show>
        </form>
      </Modal>

      {/* Edit */}
      <Modal
        open={editing() !== null}
        title={`Edit ${editing()?.name ?? ''}`}
        onClose={() => setEditing(null)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setEditing(null)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleEdit} disabled={busy()}>
              {busy() ? 'Saving…' : 'Save changes'}
            </button>
          </>
        }
      >
        <form onSubmit={handleEdit}>
          <div class="field">
            <label class="field-label">Visibility</label>
            <select class="select" value={editVisibility()} onChange={(e) => setEditVisibility(e.currentTarget.value)}>
              <option value="private">private</option>
              <option value="public">public</option>
            </select>
          </div>
          <Show when={editing()?.type === 'proxy'}>
            <div class="field">
              <label class="field-label">Upstream URL</label>
              <input class="input" value={editUpstream()} onInput={(e) => setEditUpstream(e.currentTarget.value)} spellcheck={false} />
            </div>
          </Show>
          <Show when={editing()?.type === 'group'}>
            <div class="field">
              <label class="field-label">Members (resolution order)</label>
              <input
                class="input"
                value={editMembers()}
                onInput={(e) => setEditMembers(e.currentTarget.value)}
                placeholder="leave blank to keep current"
                spellcheck={false}
              />
            </div>
          </Show>
        </form>
      </Modal>

      <ConfirmModal
        open={deleting() !== null}
        title={`Delete ${deleting()}?`}
        message="Only empty repositories can be deleted; the API refuses otherwise. Clients using this endpoint will start failing."
        confirmLabel="Delete repository"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeleting(null)}
      />
    </div>
  );
}
