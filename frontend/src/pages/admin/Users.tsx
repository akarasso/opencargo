import { createSignal, createResource, For, Show } from 'solid-js';
import { A } from '@solidjs/router';
import { fetchUsers, createUser, updateUser, deleteUser, type User } from '../../lib/api.ts';
import { ConfirmModal } from '../../components/Modal.tsx';
import Modal from '../../components/Modal.tsx';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';
import EmptyState from '../../components/EmptyState.tsx';
import { toast } from '../../components/Toast.tsx';

function getRoleBadgeClass(role: string): string {
  switch (role) {
    case 'admin': return 'badge badge-role-admin';
    case 'publisher': return 'badge badge-role-publisher';
    default: return 'badge badge-role-reader';
  }
}

function getInitials(username: string): string {
  const parts = username.split(/[._-]/);
  if (parts.length >= 2) {
    return (parts[0][0] + parts[1][0]).toUpperCase();
  }
  return username.slice(0, 2).toUpperCase();
}

export default function Users() {
  const [users, { refetch }] = createResource(fetchUsers);
  const [showCreate, setShowCreate] = createSignal(false);
  const [editingUser, setEditingUser] = createSignal<User | null>(null);
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
    setCreateLoading(true);
    try {
      await createUser({ username: newUsername(), email: newEmail() || undefined, password: newPassword(), role: newRole() });
      toast.success(`User "${newUsername()}" created.`);
      setShowCreate(false);
      setNewUsername(''); setNewEmail(''); setNewPassword(''); setNewRole('reader');
      refetch();
    } catch (err: any) { toast.error(err.message || 'Failed to create user.'); }
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
      toast.success(`User "${user.username}" updated.`);
      setEditingUser(null);
      refetch();
    } catch (err: any) { toast.error(err.message || 'Failed to update user.'); }
    setEditLoading(false);
  }

  async function handleDelete() {
    const username = deletingUser();
    if (!username) return;
    try {
      await deleteUser(username);
      toast.success(`User "${username}" deleted.`);
      setDeletingUser(null);
      refetch();
    } catch (err: any) { toast.error(err.message || 'Failed to delete user.'); }
  }

  return (
    <>
      {/* Page Header -- matches Stitch users page */}
      <div style={{ display: 'flex', "align-items": 'flex-end', "justify-content": 'space-between', "margin-bottom": '2rem' }}>
        <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
          <div style={{ display: 'flex', "align-items": 'center', gap: '0.5rem', "font-size": '0.625rem', "font-family": 'var(--font-label)', "letter-spacing": '0.3em', color: 'rgba(123, 231, 249, 0.6)', "text-transform": 'uppercase' }}>
            <span style={{ width: '8px', height: '8px', "border-radius": '50%', background: 'var(--clr-primary)' }} class="status-led-animated" />
            LIVE_SYSTEM_RESOURCES
          </div>
          <h1 class="page-title">User Management</h1>
        </div>
        <button class="btn btn-primary" onClick={() => setShowCreate(true)}>
          <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>person_add</span>
          Create User
        </button>
      </div>

      <Show when={users.loading}><LoadingSpinner /></Show>
      <Show when={users.error}><div class="alert alert-error">Failed to load users. Make sure you are logged in as an admin.</div></Show>

      <Show when={users()}>
        {(list) => (
          <Show when={list().length > 0} fallback={<EmptyState title="No users" text="No users found." />}>
            {/* Stats Grid -- matches Stitch users page */}
            <div style={{ display: 'grid', "grid-template-columns": 'repeat(4, 1fr)', gap: '1rem', "margin-bottom": '2rem' }}>
              <div style={{ background: 'var(--clr-surface-container)', padding: '1rem', display: 'flex', "flex-direction": 'column', gap: '0.25rem', "border-left": '2px solid rgba(123, 231, 249, 0.4)' }}>
                <span style={{ "font-size": '0.625rem', "font-weight": '700', color: 'rgb(100, 116, 139)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>Total Identities</span>
                <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.5rem', "font-weight": '700' }}>{list().length.toLocaleString()}</span>
              </div>
              <div style={{ background: 'var(--clr-surface-container)', padding: '1rem', display: 'flex', "flex-direction": 'column', gap: '0.25rem', "border-left": '2px solid rgba(123, 231, 249, 0.2)' }}>
                <span style={{ "font-size": '0.625rem', "font-weight": '700', color: 'rgb(100, 116, 139)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>Admin Users</span>
                <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.5rem', "font-weight": '700', color: 'var(--clr-primary)' }}>{list().filter(u => u.role === 'admin').length}</span>
              </div>
              <div style={{ background: 'var(--clr-surface-container)', padding: '1rem', display: 'flex', "flex-direction": 'column', gap: '0.25rem', "border-left": '2px solid rgba(16, 213, 255, 0.2)' }}>
                <span style={{ "font-size": '0.625rem', "font-weight": '700', color: 'rgb(100, 116, 139)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>Publishers</span>
                <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.5rem', "font-weight": '700' }}>{list().filter(u => u.role === 'publisher').length}</span>
              </div>
              <div style={{ background: 'var(--clr-surface-container)', padding: '1rem', display: 'flex', "flex-direction": 'column', gap: '0.25rem', "border-left": '2px solid rgba(255, 113, 108, 0.2)' }}>
                <span style={{ "font-size": '0.625rem', "font-weight": '700', color: 'rgb(100, 116, 139)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>Readers</span>
                <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.5rem', "font-weight": '700' }}>{list().filter(u => u.role === 'reader').length}</span>
              </div>
            </div>

            {/* User Registry Table -- matches Stitch users page */}
            <div class="data-table-wrapper">
              <div style={{ "overflow-x": 'auto' }}>
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Username</th>
                      <th>Email Path</th>
                      <th>Privilege Level</th>
                      <th>Created_TS</th>
                      <th style={{ "text-align": 'right' }}>Interaction</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={list()}>
                      {(user) => (
                        <tr>
                          <td>
                            <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                              <div style={{ width: '8px', height: '8px', "border-radius": '50%', background: user.role === 'admin' ? 'var(--clr-primary)' : user.role === 'publisher' ? 'rgba(138, 184, 255, 0.4)' : 'rgb(75, 85, 99)' }} />
                              <span style={{ "font-family": 'var(--font-headline)', "font-weight": '700', "font-size": '0.875rem', "letter-spacing": '-0.025em', color: 'var(--clr-on-surface)' }}>{user.username}</span>
                            </div>
                          </td>
                          <td style={{ "font-family": 'var(--font-label)', "font-size": '0.75rem', color: 'rgb(148, 163, 184)', "letter-spacing": '-0.025em' }}>{user.email || 'none'}</td>
                          <td>
                            <span class={getRoleBadgeClass(user.role)}>{user.role}</span>
                          </td>
                          <td style={{ "font-family": 'var(--font-label)', "font-size": '0.6875rem', color: 'rgb(100, 116, 139)', "text-transform": 'uppercase', "letter-spacing": '0.2em' }}>{user.created_at}</td>
                          <td style={{ "text-align": 'right' }}>
                            <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'flex-end', gap: '0.75rem' }}>
                              <button style={{ padding: '0.25rem', background: 'none', border: 'none', color: 'rgb(100, 116, 139)', cursor: 'pointer' }} onClick={() => openEdit(user)}>
                                <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>edit</span>
                              </button>
                              <A href={`/admin/users/${user.username}/tokens`} style={{ padding: '0.25rem', color: 'rgb(100, 116, 139)', "text-decoration": 'none' }}>
                                <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>key</span>
                              </A>
                              <button style={{ padding: '0.25rem', background: 'none', border: 'none', color: 'rgb(100, 116, 139)', cursor: 'pointer' }} onClick={() => setDeletingUser(user.username)}>
                                <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>delete</span>
                              </button>
                            </div>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
              <div style={{ "margin-top": 'auto', "border-top": '1px solid rgba(67, 72, 78, 0.1)', padding: '1rem', display: 'flex', "align-items": 'center', "justify-content": 'space-between' }}>
                <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'rgb(75, 85, 99)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>Registry Page 1</span>
              </div>
            </div>
          </Show>
        )}
      </Show>

      {/* Create user modal */}
      <Modal open={showCreate()} title="Create User" onClose={() => setShowCreate(false)}
        actions={<><button class="btn btn-secondary" onClick={() => setShowCreate(false)}>Cancel</button><button class="btn btn-primary" onClick={handleCreate} disabled={createLoading()}>{createLoading() ? 'Creating...' : 'Create'}</button></>}>
        <form onSubmit={handleCreate}>
          <div class="form-group"><label class="form-label">Username</label><input class="form-input" type="text" value={newUsername()} onInput={(e) => setNewUsername(e.currentTarget.value)} required /></div>
          <div class="form-group"><label class="form-label">Email</label><input class="form-input" type="email" value={newEmail()} onInput={(e) => setNewEmail(e.currentTarget.value)} /></div>
          <div class="form-group"><label class="form-label">Password</label><input class="form-input" type="password" value={newPassword()} onInput={(e) => setNewPassword(e.currentTarget.value)} required /></div>
          <div class="form-group"><label class="form-label">Role</label><select class="form-select" value={newRole()} onChange={(e) => setNewRole(e.currentTarget.value)}><option value="reader">reader</option><option value="publisher">publisher</option><option value="admin">admin</option></select></div>
        </form>
      </Modal>

      {/* Edit user modal */}
      <Modal open={editingUser() !== null} title={`Edit User: ${editingUser()?.username || ''}`} onClose={() => setEditingUser(null)}
        actions={<><button class="btn btn-secondary" onClick={() => setEditingUser(null)}>Cancel</button><button class="btn btn-primary" onClick={handleEdit} disabled={editLoading()}>{editLoading() ? 'Saving...' : 'Save'}</button></>}>
        <form onSubmit={handleEdit}>
          <div class="form-group"><label class="form-label">Email</label><input class="form-input" type="email" value={editEmail()} onInput={(e) => setEditEmail(e.currentTarget.value)} /></div>
          <div class="form-group"><label class="form-label">Role</label><select class="form-select" value={editRole()} onChange={(e) => setEditRole(e.currentTarget.value)}><option value="reader">reader</option><option value="publisher">publisher</option><option value="admin">admin</option></select></div>
          <div class="form-group"><label class="form-label">New Password</label><input class="form-input" type="password" value={editPassword()} onInput={(e) => setEditPassword(e.currentTarget.value)} placeholder="Leave blank to keep current" /><div class="form-hint">Leave empty to keep current password.</div></div>
        </form>
      </Modal>

      <ConfirmModal open={deletingUser() !== null} title="Delete User" message={`Are you sure you want to delete the user "${deletingUser()}"? This action cannot be undone.`} confirmLabel="Delete User" danger onConfirm={handleDelete} onCancel={() => setDeletingUser(null)} />
    </>
  );
}
