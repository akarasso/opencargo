import { For, Show, createSignal } from 'solid-js';
import { A } from '@solidjs/router';
import Icon from '../../components/Icon.tsx';
import RoleBadge from '../../components/RoleBadge.tsx';
import Modal, { ConfirmModal } from '../../components/Modal.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import PermissionMatrix from '../../components/PermissionMatrix.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import { createUser, deleteUser, fetchUsers, updateUser } from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';
import { session } from '../../core/stores/session.ts';
import { toasts } from '../../core/stores/toasts.ts';
import { initials, timeAgo } from '../../core/format.ts';
import type { User } from '../../core/types.ts';

export default function Users() {
  return (
    <RequireAdmin>
      <UsersInner />
    </RequireAdmin>
  );
}

function UsersInner() {
  const [users, refetch] = createLiveResource(fetchUsers, ['audit.entry', 'permissions.changed']);

  const [showCreate, setShowCreate] = createSignal(false);
  const [editingUser, setEditingUser] = createSignal<User | null>(null);
  const [accessUser, setAccessUser] = createSignal<User | null>(null);
  const [deletingUser, setDeletingUser] = createSignal<string | null>(null);

  const [newUsername, setNewUsername] = createSignal('');
  const [newEmail, setNewEmail] = createSignal('');
  const [newPassword, setNewPassword] = createSignal('');
  const [newRole, setNewRole] = createSignal('reader');
  const [createLoading, setCreateLoading] = createSignal(false);

  const [editEmail, setEditEmail] = createSignal('');
  const [editRole, setEditRole] = createSignal('');
  const [editPassword, setEditPassword] = createSignal('');
  const [editLoading, setEditLoading] = createSignal(false);

  async function handleCreate(e: Event) {
    e.preventDefault();
    if (!newUsername() || !newPassword()) return;
    setCreateLoading(true);
    try {
      await createUser({
        username: newUsername(),
        email: newEmail() || undefined,
        password: newPassword(),
        role: newRole(),
      });
      toasts.success(`User ${newUsername()} created`, `role: ${newRole()}`);
      setShowCreate(false);
      setNewUsername('');
      setNewEmail('');
      setNewPassword('');
      setNewRole('reader');
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not create user', err instanceof Error ? err.message : undefined);
    }
    setCreateLoading(false);
  }

  function openEdit(user: User) {
    setEditingUser(user);
    setEditEmail(user.email || '');
    setEditRole(user.role);
    setEditPassword('');
  }

  async function handleEdit(e: Event) {
    e.preventDefault();
    const user = editingUser();
    if (!user) return;
    setEditLoading(true);
    try {
      const data: { email?: string; password?: string; role?: string } = {};
      if (editEmail() !== (user.email || '')) data.email = editEmail();
      if (editRole() !== user.role) data.role = editRole();
      if (editPassword()) data.password = editPassword();
      await updateUser(user.username, data);
      toasts.success(`User ${user.username} updated`);
      setEditingUser(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not update user', err instanceof Error ? err.message : undefined);
    }
    setEditLoading(false);
  }

  async function handleDelete() {
    const username = deletingUser();
    if (!username) return;
    try {
      await deleteUser(username);
      toasts.success(`User ${username} deleted`);
      setDeletingUser(null);
      void refetch();
    } catch (err: unknown) {
      toasts.error('Could not delete user', err instanceof Error ? err.message : undefined);
    }
  }

  const roleCount = (role: string) => users()?.filter((u) => u.role === role).length ?? 0;

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Users & access</h1>
          <p class="page-sub">
            Accounts, roles, and per-repository rights. Changes apply to open sessions instantly.
          </p>
        </div>
        <div class="page-actions">
          <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
            <Icon name="plus" size={14} />
            Add user
          </button>
        </div>
      </div>

      <Show when={users.error}>
        <LoadError what="users" />
      </Show>

      <Show when={users()} fallback={<TableSkeleton rows={5} cols={5} />}>
        {(list) => (
          <Show
            when={list().length > 0}
            fallback={
              <div class="card">
                <EmptyState icon="users" title="No users yet" text="Add the first account to hand out access." />
              </div>
            }
          >
            <div class="row-wrap" style={{ 'margin-bottom': '14px' }}>
              <span class="chip chip-accent">{roleCount('admin')} admin</span>
              <span class="chip chip-info">{roleCount('publisher')} publisher</span>
              <span class="chip chip-neutral">{roleCount('reader')} reader</span>
            </div>

            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>User</th>
                      <th class="cell-hide-sm">Email</th>
                      <th>Role</th>
                      <th class="cell-hide-sm">Created</th>
                      <th style={{ 'text-align': 'right' }}>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={list()}>
                      {(user) => (
                        <tr>
                          <td>
                            <div class="row">
                              <div class="avatar" style={{ width: '28px', height: '28px', 'font-size': '0.62rem' }}>
                                {initials(user.username)}
                              </div>
                              <span style={{ 'font-weight': 600, color: 'var(--ink)' }}>{user.username}</span>
                              <Show when={user.username === session.user()?.username}>
                                <span class="chip chip-neutral">you</span>
                              </Show>
                            </div>
                          </td>
                          <td class="cell-muted cell-hide-sm">{user.email || '—'}</td>
                          <td>
                            <RoleBadge role={user.role} />
                          </td>
                          <td class="cell-dim cell-hide-sm" title={user.created_at}>
                            {timeAgo(user.created_at)}
                          </td>
                          <td>
                            <div class="cell-actions">
                              <button
                                class="btn btn-ghost btn-sm"
                                title={`Per-repository rights for ${user.username}`}
                                onClick={() => setAccessUser(user)}
                              >
                                <Icon name="shield" size={13} />
                                Access
                              </button>
                              <A
                                class="btn btn-quiet btn-icon"
                                href={`/admin/users/${user.username}/tokens`}
                                title="API tokens"
                              >
                                <Icon name="key" size={14} />
                              </A>
                              <button class="btn btn-quiet btn-icon" title="Edit user" onClick={() => openEdit(user)}>
                                <Icon name="pencil" size={14} />
                              </button>
                              <button
                                class="btn btn-quiet btn-icon"
                                title={
                                  user.username === session.user()?.username
                                    ? "You can't delete your own account"
                                    : 'Delete user'
                                }
                                disabled={user.username === session.user()?.username}
                                onClick={() => setDeletingUser(user.username)}
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
        title="Add user"
        subtitle="They can sign in on the web and with npm/docker/cargo clients right away."
        onClose={() => setShowCreate(false)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setShowCreate(false)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleCreate} disabled={createLoading()}>
              {createLoading() ? 'Creating…' : 'Create user'}
            </button>
          </>
        }
      >
        <form onSubmit={handleCreate}>
          <div class="field">
            <label class="field-label">Username</label>
            <input class="input" value={newUsername()} onInput={(e) => setNewUsername(e.currentTarget.value)} required spellcheck={false} />
          </div>
          <div class="field">
            <label class="field-label">Email (optional)</label>
            <input class="input" type="email" value={newEmail()} onInput={(e) => setNewEmail(e.currentTarget.value)} />
          </div>
          <div class="field">
            <label class="field-label">Password</label>
            <input class="input" type="password" value={newPassword()} onInput={(e) => setNewPassword(e.currentTarget.value)} required />
          </div>
          <div class="field">
            <label class="field-label">Role</label>
            <select class="select" value={newRole()} onChange={(e) => setNewRole(e.currentTarget.value)}>
              <option value="reader">reader — install only</option>
              <option value="publisher">publisher — install & publish</option>
              <option value="admin">admin — everything, everywhere</option>
            </select>
            <div class="field-hint">Fine-grained per-repository rights can be set after creation via “Access”.</div>
          </div>
        </form>
      </Modal>

      {/* Edit */}
      <Modal
        open={editingUser() !== null}
        title={`Edit ${editingUser()?.username ?? ''}`}
        onClose={() => setEditingUser(null)}
        actions={
          <>
            <button class="btn btn-ghost" onClick={() => setEditingUser(null)}>
              Cancel
            </button>
            <button class="btn btn-primary" onClick={handleEdit} disabled={editLoading()}>
              {editLoading() ? 'Saving…' : 'Save changes'}
            </button>
          </>
        }
      >
        <form onSubmit={handleEdit}>
          <div class="field">
            <label class="field-label">Email</label>
            <input class="input" type="email" value={editEmail()} onInput={(e) => setEditEmail(e.currentTarget.value)} />
          </div>
          <div class="field">
            <label class="field-label">Role</label>
            <select class="select" value={editRole()} onChange={(e) => setEditRole(e.currentTarget.value)}>
              <option value="reader">reader</option>
              <option value="publisher">publisher</option>
              <option value="admin">admin</option>
            </select>
            <div class="field-hint">Role changes reach the user's open sessions immediately.</div>
          </div>
          <div class="field">
            <label class="field-label">New password</label>
            <input
              class="input"
              type="password"
              value={editPassword()}
              onInput={(e) => setEditPassword(e.currentTarget.value)}
              placeholder="Leave blank to keep the current one"
            />
          </div>
        </form>
      </Modal>

      {/* Access matrix */}
      <Modal
        open={accessUser() !== null}
        wide
        title={`Access — ${accessUser()?.username ?? ''}`}
        subtitle="Effective rights per repository. Explicit grants replace the role default entirely."
        onClose={() => setAccessUser(null)}
      >
        <Show when={accessUser()}>
          {(u) => <PermissionMatrix username={u().username} role={u().role} />}
        </Show>
      </Modal>

      <ConfirmModal
        open={deletingUser() !== null}
        title={`Delete ${deletingUser()}?`}
        message="Their tokens stop working immediately and their per-repository grants are removed. Published packages stay."
        confirmLabel="Delete user"
        danger
        onConfirm={handleDelete}
        onCancel={() => setDeletingUser(null)}
      />
    </div>
  );
}
