import { Show } from 'solid-js';
import { A } from '@solidjs/router';
import Icon from './Icon.tsx';
import RoleBadge from './RoleBadge.tsx';
import { session } from '../core/stores/session.ts';
import { ui } from '../core/stores/ui.ts';
import { initials } from '../core/format.ts';

export default function Sidebar() {
  const close = ui.closeSidebar;

  return (
    <>
      <div
        class={`drawer-overlay ${ui.sidebarOpen() ? 'visible' : ''}`}
        onClick={close}
        aria-hidden="true"
      />
      <aside class={`sidebar ${ui.sidebarOpen() ? 'open' : ''}`}>
        <div class="brand">
          <div class="brand-mark">
            <Icon name="anchor" size={18} strokeWidth={2} />
          </div>
          <div>
            <div class="brand-name">
              <A href="/" onClick={close}>
                OpenCargo
              </A>
            </div>
            <div class="brand-sub">package registry</div>
          </div>
        </div>

        <div class="nav-section">
          <div class="nav-label">Browse</div>
          <nav class="nav">
            <A class="nav-link" href="/" end activeClass="active" onClick={close}>
              <Icon name="gauge" />
              <span>Dashboard</span>
            </A>
            <A class="nav-link" href="/packages" activeClass="active" onClick={close}>
              <Icon name="package" />
              <span>Packages</span>
            </A>
            <A class="nav-link" href="/search" activeClass="active" onClick={close}>
              <Icon name="search" />
              <span>Search</span>
            </A>
            <A class="nav-link" href="/oci" activeClass="active" onClick={close}>
              <Icon name="container" />
              <span>Containers</span>
            </A>
            <A class="nav-link" href="/go" activeClass="active" onClick={close}>
              <Icon name="code" />
              <span>Go modules</span>
            </A>
          </nav>
        </div>

        {/* Account section for any signed-in user */}
        <Show when={session.isAuthenticated()}>
          <div class="nav-section">
            <div class="nav-label">Account</div>
            <nav class="nav">
              <A class="nav-link" href="/account/access" activeClass="active" onClick={close}>
                <Icon name="shield-check" />
                <span>My access</span>
              </A>
              <A
                class="nav-link"
                href={`/admin/users/${session.user()?.username}/tokens`}
                activeClass="active"
                onClick={close}
              >
                <Icon name="key" />
                <span>API tokens</span>
              </A>
              <A class="nav-link" href="/admin/password" activeClass="active" onClick={close}>
                <Icon name="lock" />
                <span>Password</span>
              </A>
            </nav>
          </div>
        </Show>

        {/* Admin section, only for the admin role */}
        <Show when={session.isAdmin()}>
          <div class="nav-section">
            <div class="nav-label">Administration</div>
            <nav class="nav">
              <A class="nav-link" href="/admin" end activeClass="active" onClick={close}>
                <Icon name="activity" />
                <span>Overview</span>
              </A>
              <A class="nav-link" href="/admin/repositories" activeClass="active" onClick={close}>
                <Icon name="database" />
                <span>Repositories</span>
              </A>
              <A class="nav-link" href="/admin/users" activeClass="active" onClick={close}>
                <Icon name="users" />
                <span>Users & access</span>
              </A>
              <A class="nav-link" href="/admin/packages" activeClass="active" onClick={close}>
                <Icon name="layers" />
                <span>Promotion</span>
              </A>
              <A class="nav-link" href="/admin/webhooks" activeClass="active" onClick={close}>
                <Icon name="webhook" />
                <span>Webhooks</span>
              </A>
              <A class="nav-link" href="/admin/audit" activeClass="active" onClick={close}>
                <Icon name="history" />
                <span>Audit log</span>
              </A>
              <A class="nav-link" href="/admin/system" activeClass="active" onClick={close}>
                <Icon name="settings" />
                <span>System</span>
              </A>
            </nav>
          </div>
        </Show>

        <div class="sidebar-foot">
          <Show
            when={session.isAuthenticated()}
            fallback={
              <A class="signin-cta" href="/login" onClick={close}>
                <Icon name="log-in" size={15} />
                Sign in
              </A>
            }
          >
            <div class="user-chip">
              <div class="avatar">{initials(session.user()?.username)}</div>
              <div class="grow">
                <div class="user-chip-name">{session.user()?.username}</div>
                <div class="user-chip-meta">
                  <RoleBadge role={session.user()?.role ?? 'reader'} />
                </div>
              </div>
              <button
                class="btn btn-quiet btn-icon"
                title="Sign out"
                aria-label="Sign out"
                onClick={() => {
                  session.logout();
                  close();
                }}
              >
                <Icon name="log-out" size={15} />
              </button>
            </div>
          </Show>
        </div>
      </aside>
    </>
  );
}
