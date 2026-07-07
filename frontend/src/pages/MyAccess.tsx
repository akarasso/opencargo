import { For, Show } from 'solid-js';
import Icon from '../components/Icon.tsx';
import RoleBadge from '../components/RoleBadge.tsx';
import { RequireAuth } from '../components/guards.tsx';
import { FormatTag, PermChips, TableSkeleton, VisibilityChip } from '../components/bits.tsx';
import { session } from '../core/stores/session.ts';

const SOURCE_LABEL: Record<string, string> = {
  admin: 'admin role — full access everywhere',
  grant: 'explicit grant on this repository',
  role: 'default for your role',
  anonymous: 'anonymous read',
};

export default function MyAccess() {
  return (
    <RequireAuth>
      <div class="page-enter">
        <div class="page-head">
          <div>
            <h1 class="page-title">My access</h1>
            <p class="page-sub">
              What you can do on each repository, and which rule grants it. Rights change here the
              moment an admin updates them.
            </p>
          </div>
          <div class="row">
            <span class="dim small">Signed in as</span>
            <span class="mono">{session.user()?.username}</span>
            <RoleBadge role={session.user()?.role ?? 'reader'} />
          </div>
        </div>

        <Show when={session.permissions()} fallback={<TableSkeleton rows={4} cols={4} />}>
          {(mine) => (
            <div class="table-card page-enter">
              <div class="table-scroll">
                <table class="table">
                  <thead>
                    <tr>
                      <th>Repository</th>
                      <th>Format</th>
                      <th>Visibility</th>
                      <th>Your rights</th>
                      <th class="cell-hide-sm">Granted by</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={mine().permissions}>
                      {(p) => (
                        <tr>
                          <td>
                            <span class="mono" style={{ color: 'var(--ink)' }}>
                              {p.repository}
                            </span>
                          </td>
                          <td>
                            <FormatTag format={p.format} />
                          </td>
                          <td>
                            <VisibilityChip visibility={p.visibility} />
                          </td>
                          <td>
                            <PermChips perms={p} />
                          </td>
                          <td class="cell-hide-sm">
                            <span class="source-note" title={SOURCE_LABEL[p.source]}>
                              {p.source}
                            </span>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </div>
          )}
        </Show>

        <div class="alert alert-info" style={{ 'margin-top': '16px' }}>
          <Icon name="info" size={15} />
          <span>
            <strong>read</strong> lets you install, <strong>write</strong> lets you publish,{' '}
            <strong>delete</strong> covers yank and removal, <strong>admin</strong> manages the
            repository itself. Need more? Ask an administrator to grant it per repository.
          </span>
        </div>
      </div>
    </RequireAuth>
  );
}
