import { Show, createEffect } from 'solid-js';
import { useLocation, useNavigate, type RouteSectionProps } from '@solidjs/router';
import Sidebar from './Sidebar.tsx';
import Icon from './Icon.tsx';
import CommandPalette from './CommandPalette.tsx';
import { session } from '../core/stores/session.ts';
import { ui } from '../core/stores/ui.ts';
import { wsStatus } from '../core/ws.ts';

const STATUS_LABEL = {
  online: 'Live',
  connecting: 'Connecting',
  offline: 'Offline',
} as const;

export default function Layout(props: RouteSectionProps) {
  const location = useLocation();
  const navigate = useNavigate();

  // Forced password rotation: everything except the password page 403s
  // server-side, so route the user straight to the one page that works.
  createEffect(() => {
    const u = session.user();
    if (u?.mustChangePassword && location.pathname !== '/admin/password') {
      navigate('/admin/password', { replace: true });
    }
  });

  return (
    <div class="shell">
      <Sidebar />
      <div class="main">
        <header class="topbar">
          <button
            class="topbar-burger"
            onClick={ui.toggleSidebar}
            aria-label="Open navigation"
          >
            <Icon name="menu" size={18} />
          </button>

          <button class="search-trigger" onClick={ui.openPalette}>
            <Icon name="search" size={14} />
            <span class="truncate">Search packages…</span>
            <span class="kbd-group">
              <span class="kbd">⌘</span>
              <span class="kbd">K</span>
            </span>
          </button>

          <div class="topbar-spacer" />

          <div class={`conn ${wsStatus()}`} title="Real-time connection">
            <span class="conn-dot" />
            <span class="conn-text">{STATUS_LABEL[wsStatus()]}</span>
          </div>
        </header>

        <main class="content">{props.children}</main>

        <footer class="footer">
          <span>opencargo — self-hosted package registry</span>
          <Show when={session.user()}>
            {(u) => (
              <span>
                {u().username} · {u().role}
              </span>
            )}
          </Show>
        </footer>
      </div>

      <CommandPalette />
    </div>
  );
}
