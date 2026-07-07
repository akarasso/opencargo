import { For, Show, createSignal, onCleanup, onMount } from 'solid-js';
import { A } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import CountUp from '../components/CountUp.tsx';
import CopyButton from '../components/CopyButton.tsx';
import EmptyState from '../components/EmptyState.tsx';
import { FormatTag, StatsSkeleton, LoadError, VisibilityChip } from '../components/bits.tsx';
import { fetchDashboard, fetchRepositories } from '../core/api.ts';
import { createLiveResource } from '../core/stores/live.ts';
import { session } from '../core/stores/session.ts';
import { onEvent, wsStatus } from '../core/ws.ts';
import { timeAgo } from '../core/format.ts';

interface ManifestRow {
  key: string;
  kind: 'published' | 'promoted';
  pkg: string;
  version: string;
  repo?: string;
  detail?: string;
  at: string;
  fresh?: boolean;
}

export default function Dashboard() {
  const [data] = createLiveResource(fetchDashboard, [
    'package.published',
    'package.promoted',
    'registry.changed',
    'repositories.changed',
  ]);
  const [repos] = createLiveResource(fetchRepositories, ['repositories.changed']);

  // The live manifest: seeded from recent_versions, prepended by WS events.
  const [liveRows, setLiveRows] = createSignal<ManifestRow[]>([]);

  onMount(() => {
    const unsubs = [
      onEvent('package.published', (ev) => {
        const d = ev.data ?? {};
        setLiveRows((rows) =>
          [
            {
              key: `${ev.ts}-${d.package}`,
              kind: 'published' as const,
              pkg: String(d.package ?? 'package'),
              version: String(d.version ?? ''),
              repo: d.repository ? String(d.repository) : undefined,
              at: ev.ts ?? '',
              fresh: true,
            },
            ...rows,
          ].slice(0, 14),
        );
      }),
      onEvent('package.promoted', (ev) => {
        const d = ev.data ?? {};
        setLiveRows((rows) =>
          [
            {
              key: `${ev.ts}-${d.package}-promo`,
              kind: 'promoted' as const,
              pkg: String(d.package ?? 'package'),
              version: String(d.version ?? ''),
              // `from` is omitted for private source repos.
              detail: d.from ? `${d.from} → ${d.to}` : `→ ${d.to ?? '?'}`,
              at: ev.ts ?? '',
              fresh: true,
            },
            ...rows,
          ].slice(0, 14),
        );
      }),
    ];
    onCleanup(() => unsubs.forEach((u) => u()));
  });

  const manifest = (): ManifestRow[] => {
    const seeded: ManifestRow[] = (data()?.recent_versions ?? []).map((rv) => ({
      key: `db-${rv.package_name}-${rv.version}`,
      kind: 'published',
      pkg: rv.package_name,
      version: rv.version,
      at: rv.published_at,
    }));
    const live = liveRows();
    const seen = new Set(live.map((r) => `${r.pkg}@${r.version}`));
    return [...live, ...seeded.filter((r) => !seen.has(`${r.pkg}@${r.version}`))].slice(0, 12);
  };

  const base = () => `${location.protocol}//${location.host}`;

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">Dashboard</h1>
          <p class="page-sub">What's moving through your registry, as it happens.</p>
        </div>
      </div>

      <Show when={data.error}>
        <LoadError what="registry stats" />
      </Show>

      <div class="stagger">
        <Show when={data()} fallback={<StatsSkeleton />}>
          {(d) => (
            <section class="stats-grid section">
              <div class="stat" style={{ '--stat-tint': 'var(--accent)' }}>
                <div class="stat-head">
                  <span class="stat-label">Packages</span>
                  <Icon name="package" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_packages} />
                </div>
                <div class="stat-foot">across {d().total_repos} repositories</div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--steel)' }}>
                <div class="stat-head">
                  <span class="stat-label">Versions</span>
                  <Icon name="layers" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_versions} />
                </div>
                <div class="stat-foot">immutable, content-addressed</div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--ok)' }}>
                <div class="stat-head">
                  <span class="stat-label">Downloads</span>
                  <Icon name="download" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_downloads} />
                </div>
                <div class="stat-foot">served from this registry</div>
              </div>
              <div class="stat" style={{ '--stat-tint': 'var(--fmt-go)' }}>
                <div class="stat-head">
                  <span class="stat-label">Repositories</span>
                  <Icon name="database" size={16} />
                </div>
                <div class="stat-value">
                  <CountUp value={d().total_repos} />
                </div>
                <div class="stat-foot">hosted · proxy · group</div>
              </div>
            </section>
          )}
        </Show>

        <section class="section dashboard-cols">
          {/* Live manifest — the ship's log */}
          <div class="feed-card">
            <div class="feed-head">
              <div class="row">
                <Icon name="activity" size={15} class="icon" />
                <span class="section-title">Manifest</span>
              </div>
              <span class={`feed-live ${wsStatus() === 'online' ? 'online' : ''}`}>
                <span class="conn-dot" />
                {wsStatus() === 'online' ? 'live' : 'log'}
              </span>
            </div>
            <Show
              when={!data.loading || manifest().length > 0}
              fallback={
                <ul class="feed">
                  <For each={[0, 1, 2, 3, 4]}>
                    {() => (
                      <li class="feed-row">
                        <div class="skeleton skeleton-text" style={{ width: '64px' }} />
                        <div class="skeleton skeleton-text grow" style={{ 'max-width': '45%' }} />
                        <div class="skeleton skeleton-text" style={{ width: '52px' }} />
                      </li>
                    )}
                  </For>
                </ul>
              }
            >
              <Show
                when={manifest().length > 0}
                fallback={
                  <EmptyState
                    icon="activity"
                    title="Nothing shipped yet"
                    text="Publish a package and it will appear here the moment it lands."
                  />
                }
              >
                <ul class="feed">
                  <For each={manifest()}>
                    {(row) => (
                      <li class={`feed-row ${row.fresh ? 'fresh' : ''}`}>
                        <span class="feed-time" title={row.at}>
                          {timeAgo(row.at)}
                        </span>
                        <div class="feed-main">
                          <A class="feed-pkg" href={`/packages/${row.pkg}`}>
                            {row.pkg}
                          </A>
                          <span class="version">{row.version}</span>
                          <Show when={row.kind === 'promoted'}>
                            <span class="chip chip-accent">
                              <Icon name="arrow-up-right" size={11} />
                              {row.detail}
                            </span>
                          </Show>
                          <Show when={row.repo}>
                            <span class="dim small">{row.repo}</span>
                          </Show>
                        </div>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
            </Show>
          </div>

          {/* Side column: repositories + connect */}
          <div class="col" style={{ gap: '16px' }}>
            <div class="card">
              <div class="feed-head">
                <span class="section-title">Repositories</span>
                <Show when={session.isAdmin()}>
                  <A href="/admin/repositories" class="btn btn-quiet btn-sm">
                    Manage
                  </A>
                </Show>
              </div>
              <div style={{ padding: '6px 8px 8px' }}>
                <Show
                  when={repos()}
                  fallback={
                    <div style={{ padding: '10px' }}>
                      <div class="skeleton skeleton-text" style={{ width: '80%', 'margin-bottom': '10px' }} />
                      <div class="skeleton skeleton-text" style={{ width: '65%', 'margin-bottom': '10px' }} />
                      <div class="skeleton skeleton-text" style={{ width: '72%' }} />
                    </div>
                  }
                >
                  {(r) => (
                    <Show
                      when={r().repositories.length > 0}
                      fallback={<div class="dim small" style={{ padding: '10px' }}>No repositories visible.</div>}
                    >
                      <For each={r().repositories.slice(0, 6)}>
                        {(repo) => (
                          <div class="row" style={{ padding: '7px 10px', 'justify-content': 'space-between' }}>
                            <A class="mono small grow truncate" href={`/packages?repo=${repo.name}`} style={{ color: 'var(--ink)' }}>
                              {repo.name}
                            </A>
                            <FormatTag format={repo.format} />
                            <VisibilityChip visibility={repo.visibility} />
                          </div>
                        )}
                      </For>
                    </Show>
                  )}
                </Show>
              </div>
            </div>

            <div class="card card-pad">
              <div class="section-title" style={{ 'margin-bottom': '10px' }}>
                Connect a client
              </div>
              <div class="col" style={{ gap: '10px' }}>
                <div>
                  <div class="side-label">npm / pnpm</div>
                  <div class="code-line">
                    <code>
                      <span class="accent">registry=</span>{base()}/npm-all/
                    </code>
                    <CopyButton text={`registry=${base()}/npm-all/`} label="" />
                  </div>
                </div>
                <div>
                  <div class="side-label">docker</div>
                  <div class="code-line">
                    <code>docker login {location.host}</code>
                    <CopyButton text={`docker login ${location.host}`} label="" />
                  </div>
                </div>
                <div>
                  <div class="side-label">go</div>
                  <div class="code-line">
                    <code>GOPROXY={base()}/go-private,direct</code>
                    <CopyButton text={`GOPROXY=${base()}/go-private,direct`} label="" />
                  </div>
                </div>
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}
