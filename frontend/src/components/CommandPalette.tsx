import { For, Show, createEffect, createMemo, createResource, createSignal, onCleanup, onMount } from 'solid-js';
import { useNavigate } from '@solidjs/router';
import Icon from './Icon.tsx';
import { fetchSearch } from '../core/api.ts';
import { session } from '../core/stores/session.ts';
import { ui } from '../core/stores/ui.ts';

interface Command {
  id: string;
  label: string;
  detail?: string;
  icon: string;
  group: 'Packages' | 'Go to' | 'Actions';
  run: () => void;
}

export default function CommandPalette() {
  const navigate = useNavigate();
  const [query, setQuery] = createSignal('');
  const [selected, setSelected] = createSignal(0);
  let inputRef: HTMLInputElement | undefined;

  // Debounced package search against the registry.
  const [debounced, setDebounced] = createSignal('');
  let timer: ReturnType<typeof setTimeout> | null = null;
  createEffect(() => {
    const q = query();
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => setDebounced(q), 180);
  });
  onCleanup(() => timer && clearTimeout(timer));

  const [results] = createResource(debounced, (q) =>
    q.trim() ? fetchSearch(q.trim()) : Promise.resolve({ query: '', results: [] }),
  );

  const navCommands = createMemo<Command[]>(() => {
    const go = (path: string) => () => navigate(path);
    const items: Command[] = [
      { id: 'nav-dash', label: 'Dashboard', icon: 'gauge', group: 'Go to', run: go('/') },
      { id: 'nav-pkgs', label: 'Packages', icon: 'package', group: 'Go to', run: go('/packages') },
      { id: 'nav-search', label: 'Search', icon: 'search', group: 'Go to', run: go('/search') },
      { id: 'nav-oci', label: 'Containers', icon: 'container', group: 'Go to', run: go('/oci') },
      { id: 'nav-go', label: 'Go modules', icon: 'code', group: 'Go to', run: go('/go') },
    ];
    if (session.isAuthenticated()) {
      items.push({
        id: 'nav-access',
        label: 'My access',
        icon: 'shield-check',
        group: 'Go to',
        run: go('/account/access'),
      });
    }
    if (session.isAdmin()) {
      items.push(
        { id: 'nav-admin', label: 'Admin overview', icon: 'activity', group: 'Go to', run: go('/admin') },
        { id: 'nav-repos', label: 'Repositories', icon: 'database', group: 'Go to', run: go('/admin/repositories') },
        { id: 'nav-users', label: 'Users & access', icon: 'users', group: 'Go to', run: go('/admin/users') },
        { id: 'nav-audit', label: 'Audit log', icon: 'history', group: 'Go to', run: go('/admin/audit') },
        { id: 'nav-webhooks', label: 'Webhooks', icon: 'webhook', group: 'Go to', run: go('/admin/webhooks') },
      );
    }
    if (!session.isAuthenticated()) {
      items.push({ id: 'act-login', label: 'Sign in', icon: 'log-in', group: 'Actions', run: go('/login') });
    } else {
      items.push({
        id: 'act-logout',
        label: 'Sign out',
        icon: 'log-out',
        group: 'Actions',
        run: () => session.logout(),
      });
    }
    return items;
  });

  const visible = createMemo<Command[]>(() => {
    const q = query().trim().toLowerCase();
    const pkgCommands: Command[] = (results()?.results ?? []).slice(0, 6).map((r) => ({
      id: `pkg-${r.name}`,
      label: r.name,
      detail: r.latest_version,
      icon: 'package',
      group: 'Packages' as const,
      run: () => navigate(`/packages/${r.name}`),
    }));
    const nav = q
      ? navCommands().filter((c) => c.label.toLowerCase().includes(q))
      : navCommands();
    return [...pkgCommands, ...nav];
  });

  createEffect(() => {
    visible();
    setSelected(0);
  });

  // Global shortcut: ⌘K / Ctrl+K toggles, Escape closes.
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        ui.paletteOpen() ? ui.closePalette() : ui.openPalette();
      } else if (e.key === 'Escape' && ui.paletteOpen()) {
        ui.closePalette();
      }
    };
    window.addEventListener('keydown', onKey);
    onCleanup(() => window.removeEventListener('keydown', onKey));
  });

  createEffect(() => {
    if (ui.paletteOpen()) {
      setQuery('');
      setDebounced('');
      queueMicrotask(() => inputRef?.focus());
    }
  });

  function runSelected() {
    const cmd = visible()[selected()];
    if (!cmd) return;
    ui.closePalette();
    cmd.run();
  }

  function onListKeys(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setSelected((s) => Math.min(s + 1, visible().length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setSelected((s) => Math.max(s - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      runSelected();
    }
  }

  const groups = createMemo(() => {
    const order: Command['group'][] = ['Packages', 'Go to', 'Actions'];
    return order
      .map((g) => ({ group: g, items: visible().filter((c) => c.group === g) }))
      .filter((g) => g.items.length > 0);
  });

  return (
    <Show when={ui.paletteOpen()}>
      <div class="palette-overlay" onClick={ui.closePalette}>
        <div class="palette" onClick={(e) => e.stopPropagation()}>
          <div class="palette-input-row">
            <Icon name="search" size={16} />
            <input
              ref={inputRef}
              class="palette-input"
              placeholder="Search packages or jump to…"
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
              onKeyDown={onListKeys}
              spellcheck={false}
              autocomplete="off"
            />
            <Show when={results.loading}>
              <span class="spinner" />
            </Show>
          </div>
          <div class="palette-list">
            <Show
              when={visible().length > 0}
              fallback={
                <div class="empty" style={{ padding: '26px 16px' }}>
                  <div class="empty-text">No matches for “{query()}”.</div>
                </div>
              }
            >
              <For each={groups()}>
                {(g) => (
                  <>
                    <div class="palette-group">{g.group}</div>
                    <For each={g.items}>
                      {(cmd) => {
                        const idx = () => visible().indexOf(cmd);
                        return (
                          <button
                            class={`palette-item ${selected() === idx() ? 'selected' : ''}`}
                            onMouseEnter={() => setSelected(idx())}
                            onClick={runSelected}
                          >
                            <Icon name={cmd.icon} size={15} />
                            <span class="truncate">{cmd.label}</span>
                            <Show when={cmd.detail}>
                              <span class="palette-item-detail">{cmd.detail}</span>
                            </Show>
                          </button>
                        );
                      }}
                    </For>
                  </>
                )}
              </For>
            </Show>
          </div>
          <div class="palette-foot">
            <span>
              <span class="kbd">↑↓</span> navigate
            </span>
            <span>
              <span class="kbd">↵</span> open
            </span>
            <span>
              <span class="kbd">esc</span> close
            </span>
          </div>
        </div>
      </div>
    </Show>
  );
}
