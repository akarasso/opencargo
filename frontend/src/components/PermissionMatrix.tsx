import { For, Show, createMemo, createResource } from 'solid-js';
import Icon from './Icon.tsx';
import { FormatTag, VisibilityChip } from './bits.tsx';
import {
  deleteUserPermission,
  fetchRepositories,
  fetchUserPermissions,
  setUserPermission,
} from '../core/api.ts';
import { useLive } from '../core/stores/live.ts';
import { toasts } from '../core/stores/toasts.ts';
import type { PermissionFlags, Role } from '../core/types.ts';

const ACTIONS = ['can_read', 'can_write', 'can_delete', 'can_admin'] as const;
const ACTION_LABEL: Record<(typeof ACTIONS)[number], string> = {
  can_read: 'read',
  can_write: 'write',
  can_delete: 'delete',
  can_admin: 'admin',
};

function roleDefaults(role: Role | string): PermissionFlags {
  switch (role) {
    case 'publisher':
      return { can_read: true, can_write: true, can_delete: false, can_admin: false };
    case 'reader':
      return { can_read: true, can_write: false, can_delete: false, can_admin: false };
    default:
      return { can_read: false, can_write: false, can_delete: false, can_admin: false };
  }
}

/**
 * Per-user × per-repository rights editor (admin only).
 *
 * Repos without an explicit grant show the role default, dimmed; the first
 * toggle click materializes a grant (role default with that bit flipped).
 * “Reset” removes the grant and the repo falls back to the role default.
 */
export default function PermissionMatrix(props: { username: string; role: Role | string }) {
  const [repos] = createResource(fetchRepositories);
  const [grants, { refetch }] = createResource(
    () => props.username,
    (u) => fetchUserPermissions(u),
  );
  useLive(refetch, ['permissions.changed', 'repositories.changed'], { debounce: 150 });

  const grantByRepo = createMemo(() => {
    const map = new Map<string, PermissionFlags>();
    for (const g of grants()?.permissions ?? []) {
      map.set(g.repository, {
        can_read: g.can_read,
        can_write: g.can_write,
        can_delete: g.can_delete,
        can_admin: g.can_admin,
      });
    }
    return map;
  });

  const isAdminRole = () => props.role === 'admin';
  const defaults = () => roleDefaults(props.role);

  async function toggle(repo: string, action: (typeof ACTIONS)[number]) {
    const current = grantByRepo().get(repo) ?? defaults();
    const next: PermissionFlags = { ...current, [action]: !current[action] };
    try {
      await setUserPermission(props.username, repo, next);
      void refetch();
    } catch (e: unknown) {
      toasts.error('Could not update permission', e instanceof Error ? e.message : undefined);
    }
  }

  async function reset(repo: string) {
    try {
      await deleteUserPermission(props.username, repo);
      toasts.success(`${repo} back to role default for ${props.username}`);
      void refetch();
    } catch (e: unknown) {
      toasts.error('Could not reset permission', e instanceof Error ? e.message : undefined);
    }
  }

  return (
    <div>
      <Show when={isAdminRole()}>
        <div class="alert alert-info">
          <Icon name="shield-check" size={15} />
          <span>
            <strong>{props.username}</strong> holds the admin role: full access to every
            repository. Per-repository grants only apply to publishers and readers.
          </span>
        </div>
      </Show>

      <Show
        when={repos() && grants()}
        fallback={
          <div style={{ padding: '8px 0' }}>
            <div class="skeleton skeleton-text" style={{ width: '90%', 'margin-bottom': '10px' }} />
            <div class="skeleton skeleton-text" style={{ width: '84%', 'margin-bottom': '10px' }} />
            <div class="skeleton skeleton-text" style={{ width: '88%' }} />
          </div>
        }
      >
        <div class="table-scroll">
          <table class="matrix">
            <thead>
              <tr>
                <th>Repository</th>
                <For each={[...ACTIONS]}>{(a) => <th>{ACTION_LABEL[a]}</th>}</For>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={repos()?.repositories ?? []}>
                {(repo) => {
                  const grant = () => grantByRepo().get(repo.name);
                  const effective = () =>
                    isAdminRole()
                      ? { can_read: true, can_write: true, can_delete: true, can_admin: true }
                      : (grant() ?? defaults());
                  return (
                    <tr>
                      <td>
                        <div class="matrix-repo">
                          <span class="truncate">{repo.name}</span>
                          <FormatTag format={repo.format} />
                          <VisibilityChip visibility={repo.visibility} />
                        </div>
                        <div class="source-note" style={{ 'margin-top': '2px' }}>
                          <Show when={!isAdminRole()} fallback={'admin role'}>
                            {grant() ? 'explicit grant' : `role default (${props.role})`}
                          </Show>
                        </div>
                      </td>
                      <For each={[...ACTIONS]}>
                        {(action) => (
                          <td>
                            <button
                              class={`perm-toggle ${effective()[action] ? 'on' : ''}`}
                              style={{ opacity: !isAdminRole() && !grant() ? 0.62 : 1 }}
                              disabled={isAdminRole()}
                              title={
                                isAdminRole()
                                  ? 'Admin role — always allowed'
                                  : `${effective()[action] ? 'Revoke' : 'Grant'} ${ACTION_LABEL[action]} on ${repo.name}`
                              }
                              onClick={() => toggle(repo.name, action)}
                            >
                              <Icon name={effective()[action] ? 'check' : 'x'} size={12} />
                            </button>
                          </td>
                        )}
                      </For>
                      <td>
                        <Show when={grant() && !isAdminRole()}>
                          <button
                            class="btn btn-quiet btn-sm"
                            title="Remove the explicit grant; the role default applies again"
                            onClick={() => reset(repo.name)}
                          >
                            Reset
                          </button>
                        </Show>
                      </td>
                    </tr>
                  );
                }}
              </For>
            </tbody>
          </table>
        </div>
        <p class="field-hint" style={{ 'margin-top': '10px' }}>
          Dimmed toggles show the <strong>{props.role}</strong> role default. Clicking one creates
          an explicit grant for this user on that repository; “Reset” removes it.
        </p>
      </Show>
    </div>
  );
}
