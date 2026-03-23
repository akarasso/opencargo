import { Show } from 'solid-js';
import { A } from '@solidjs/router';
import auth from '../lib/auth.ts';

interface SidebarProps {
  isOpen: boolean;
  onClose: () => void;
}

export default function Sidebar(props: SidebarProps) {
  const admin = () => auth.isAuthenticated();

  return (
    <>
      <div
        class={`sidebar-overlay ${props.isOpen ? 'sidebar-overlay-visible' : ''}`}
        onClick={props.onClose}
      />
      <aside class={`sidebar ${props.isOpen ? 'sidebar-open' : ''}`}>
        {/* Brand Header -- matches Stitch sidebar brand */}
        <div class="sidebar-brand">
          <div class="sidebar-brand-icon">
            <span class="material-symbols-outlined" style={{ "font-variation-settings": "'FILL' 1" }}>package_2</span>
          </div>
          <div class="sidebar-brand-text">
            <h1><A href="/" onClick={props.onClose}>OpenCargo</A></h1>
            <p>v1.0.4-stable</p>
          </div>
        </div>

        {/* Browse section label -- matches Stitch */}
        <div class="sidebar-section">
          <div class="sidebar-section-label">Browse</div>
          <nav class="sidebar-nav">
            <A class="sidebar-link" href="/" end activeClass="active" onClick={props.onClose}>
              <span class="material-symbols-outlined">dashboard</span>
              <span>Dashboard</span>
            </A>
            <A class="sidebar-link" href="/packages" activeClass="active" onClick={props.onClose}>
              <span class="material-symbols-outlined">package_2</span>
              <span>Packages</span>
            </A>
            <A class="sidebar-link" href="/search" activeClass="active" onClick={props.onClose}>
              <span class="material-symbols-outlined">search</span>
              <span>Search</span>
            </A>
            <A class="sidebar-link" href="/oci" activeClass="active" onClick={props.onClose}>
              <span class="material-symbols-outlined">deployed_code</span>
              <span>Containers</span>
            </A>
            <A class="sidebar-link" href="/go" activeClass="active" onClick={props.onClose}>
              <span class="material-symbols-outlined">code</span>
              <span>Go Modules</span>
            </A>
          </nav>
        </div>

        {/* Admin section (only when authenticated) -- matches Stitch admin sidebar */}
        <Show when={admin()}>
          <div class="sidebar-section">
            <div class="sidebar-section-label">Admin</div>
            <nav class="sidebar-nav">
              <A class="sidebar-link" href="/admin" end activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">speed</span>
                <span>Overview</span>
              </A>
              <A class="sidebar-link" href="/admin/repositories" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">database</span>
                <span>Repositories</span>
              </A>
              <A class="sidebar-link" href="/admin/users" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">group</span>
                <span>Users</span>
              </A>
              <A class="sidebar-link" href="/admin/packages" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">inventory_2</span>
                <span>Packages</span>
              </A>
              <A class="sidebar-link" href="/admin/audit" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">history_toggle_off</span>
                <span>Audit Log</span>
              </A>
              <A class="sidebar-link" href="/admin/system" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">settings</span>
                <span>System</span>
              </A>
              <A class="sidebar-link" href="/admin/webhooks" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">send</span>
                <span>Webhooks</span>
              </A>
              <A class="sidebar-link" href="/admin/password" activeClass="active" onClick={props.onClose}>
                <span class="material-symbols-outlined">lock</span>
                <span>Password</span>
              </A>
            </nav>
          </div>
        </Show>

        <div class="sidebar-spacer" />

        {/* Footer: Sign In button (public) or user info + logout (admin) */}
        <div class="sidebar-footer">
          <Show
            when={admin()}
            fallback={
              <A class="sidebar-signin-btn" href="/login" onClick={props.onClose}>
                Sign In
              </A>
            }
          >
            <div class="sidebar-user">
              <div class="sidebar-user-avatar">
                {(auth.username() || '?').slice(0, 2).toUpperCase()}
              </div>
              <div>
                <div class="sidebar-user-name">{auth.username()}</div>
                <div class="sidebar-user-role">Administrator</div>
              </div>
            </div>
            <button
              class="sidebar-logout-btn"
              onClick={() => {
                auth.logout();
                props.onClose();
              }}
            >
              <span class="material-symbols-outlined" style={{ "font-size": "20px" }}>logout</span>
              <span>Logout</span>
            </button>
          </Show>
        </div>
      </aside>
    </>
  );
}
