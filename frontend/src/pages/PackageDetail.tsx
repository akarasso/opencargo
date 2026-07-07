import { For, Show, createResource, createSignal } from 'solid-js';
import { useParams } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import CopyButton from '../components/CopyButton.tsx';
import EmptyState from '../components/EmptyState.tsx';
import Modal from '../components/Modal.tsx';
import { LoadError, TableSkeleton } from '../components/bits.tsx';
import {
  fetchDependencies,
  fetchDependents,
  fetchPackageDetail,
  fetchRepositories,
  fetchVulns,
  promotePackage,
  rescanVulns,
} from '../core/api.ts';
import { useLive } from '../core/stores/live.ts';
import { session } from '../core/stores/session.ts';
import { toasts } from '../core/stores/toasts.ts';
import { formatNumber, timeAgo } from '../core/format.ts';
import type { VulnReport } from '../core/types.ts';

type Tab = 'readme' | 'versions' | 'dependencies' | 'security';

const SEVERITY_CHIP: Record<string, string> = {
  critical: 'chip-danger',
  high: 'chip-danger',
  medium: 'chip-warn',
  moderate: 'chip-warn',
  low: 'chip-info',
};

export default function PackageDetail() {
  const params = useParams<{ path: string }>();
  const packageName = () => params.path;

  const [data, { refetch }] = createResource(packageName, fetchPackageDetail);
  useLive(refetch, ['package.published', 'package.promoted', 'registry.changed']);

  const [activeTab, setActiveTab] = createSignal<Tab>('readme');
  const [deps] = createResource(packageName, fetchDependencies);
  const [dependents] = createResource(packageName, fetchDependents);

  // Vulnerabilities load on first visit to the Security tab.
  const [vulnData, setVulnData] = createSignal<VulnReport | null>(null);
  const [vulnLoading, setVulnLoading] = createSignal(false);
  const [vulnError, setVulnError] = createSignal<string | null>(null);

  // Promotion
  const [showPromote, setShowPromote] = createSignal(false);
  const [promoteFrom, setPromoteFrom] = createSignal('');
  const [promoteTo, setPromoteTo] = createSignal('');
  const [promoteLoading, setPromoteLoading] = createSignal(false);
  const [repos] = createResource(fetchRepositories);

  const latestVersion = () => data()?.versions[0]?.version ?? '';
  const canPromote = () => session.canWriteAnywhere();
  const writableRepos = () =>
    (repos()?.repositories ?? []).filter(
      (r) => r.type === 'hosted' && (session.permissionFor(r.name)?.can_write ?? false),
    );
  const sourceRepos = () => (repos()?.repositories ?? []).filter((r) => r.type === 'hosted');

  async function loadVulns(force = false) {
    const d = data();
    if (!d || d.versions.length === 0) return;
    setVulnLoading(true);
    setVulnError(null);
    try {
      const fn = force ? rescanVulns : fetchVulns;
      setVulnData(await fn(d.name, d.versions[0].version));
    } catch (e: unknown) {
      setVulnError(e instanceof Error ? e.message : 'Scan unavailable');
    }
    setVulnLoading(false);
  }

  function onTabChange(tab: Tab) {
    setActiveTab(tab);
    if (tab === 'security' && !vulnData() && !vulnLoading()) void loadVulns();
  }

  async function handlePromote() {
    const d = data();
    if (!d) return;
    setPromoteLoading(true);
    try {
      await promotePackage(d.name, latestVersion(), promoteFrom(), promoteTo());
      toasts.success(`${d.name} ${latestVersion()} promoted`, `${promoteFrom()} → ${promoteTo()}`);
      setShowPromote(false);
      void refetch();
    } catch (e: unknown) {
      toasts.error('Promotion failed', e instanceof Error ? e.message : undefined);
    }
    setPromoteLoading(false);
  }

  return (
    <div class="page-enter">
      <Show when={data.error}>
        <LoadError what="this package" detail="It may not exist, or you may not have read access to its repository." />
      </Show>

      <Show
        when={data()}
        fallback={
          <Show when={!data.error}>
            <div>
              <div class="skeleton" style={{ width: '40%', height: '30px', 'margin-bottom': '10px' }} />
              <div class="skeleton skeleton-text" style={{ width: '60%', 'margin-bottom': '24px' }} />
              <TableSkeleton rows={6} cols={3} />
            </div>
          </Show>
        }
      >
        {(d) => (
          <>
            <div class="page-head">
              <div class="grow">
                <div class="row" style={{ 'margin-bottom': '4px' }}>
                  <h1 class="page-title mono" style={{ 'font-family': 'var(--font-mono)', 'font-weight': 500 }}>
                    {d().name}
                  </h1>
                  <span class="version" style={{ 'font-size': '0.8rem' }}>
                    {latestVersion()}
                  </span>
                  <Show when={d().license}>
                    <span class="chip chip-neutral">{d().license}</span>
                  </Show>
                </div>
                <Show when={d().description}>
                  <p class="page-sub">{d().description}</p>
                </Show>
              </div>
              <div class="page-actions">
                <Show when={canPromote()}>
                  <button class="btn btn-primary" onClick={() => setShowPromote(true)}>
                    <Icon name="arrow-up-right" size={14} />
                    Promote
                  </button>
                </Show>
              </div>
            </div>

            <div class="detail-grid">
              <div>
                <div class="code-line" style={{ 'margin-bottom': '18px' }}>
                  <code>
                    <span class="accent">pnpm</span> add {d().name}
                  </code>
                  <CopyButton text={`pnpm add ${d().name}`} />
                </div>

                <div class="tabs" role="tablist">
                  <For
                    each={[
                      ['readme', 'Readme'],
                      ['versions', `Versions · ${d().versions.length}`],
                      ['dependencies', 'Dependencies'],
                      ['security', 'Security'],
                    ] as [Tab, string][]}
                  >
                    {([tab, label]) => (
                      <button
                        class={`tab ${activeTab() === tab ? 'active' : ''}`}
                        role="tab"
                        aria-selected={activeTab() === tab}
                        onClick={() => onTabChange(tab)}
                      >
                        {label}
                      </button>
                    )}
                  </For>
                </div>

                {/* Readme */}
                <Show when={activeTab() === 'readme'}>
                  <Show
                    when={d().readme_html}
                    fallback={
                      <div class="card">
                        <EmptyState icon="package" title="No readme" text="This package was published without one." />
                      </div>
                    }
                  >
                    <div class="card card-pad readme" innerHTML={d().readme_html} />
                  </Show>
                </Show>

                {/* Versions */}
                <Show when={activeTab() === 'versions'}>
                  <div class="table-card">
                    <table class="table">
                      <thead>
                        <tr>
                          <th>Version</th>
                          <th>Size</th>
                          <th style={{ 'text-align': 'right' }}>Published</th>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={d().versions}>
                          {(v) => (
                            <tr>
                              <td>
                                <span class="version">{v.version}</span>
                              </td>
                              <td class="cell-mono cell-muted">{v.size_display}</td>
                              <td class="cell-dim nowrap" style={{ 'text-align': 'right' }} title={v.published_at}>
                                {timeAgo(v.published_at)}
                              </td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </div>
                </Show>

                {/* Dependencies */}
                <Show when={activeTab() === 'dependencies'}>
                  <Show when={!deps.loading} fallback={<TableSkeleton rows={4} cols={3} />}>
                    <Show
                      when={(deps() ?? []).length > 0}
                      fallback={
                        <div class="card">
                          <EmptyState icon="layers" title="No dependencies recorded" text="Nothing declared, or metadata hasn't been indexed yet." />
                        </div>
                      }
                    >
                      <div class="table-card">
                        <table class="table">
                          <thead>
                            <tr>
                              <th>Name</th>
                              <th>Requirement</th>
                              <th style={{ 'text-align': 'right' }}>Type</th>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={deps()}>
                              {(dep) => (
                                <tr>
                                  <td class="cell-mono" style={{ color: 'var(--ink)' }}>
                                    {dep.name}
                                  </td>
                                  <td class="cell-mono cell-muted">{dep.version_req}</td>
                                  <td style={{ 'text-align': 'right' }}>
                                    <span class={`chip ${dep.dep_type === 'dev' ? 'chip-neutral' : 'chip-info'}`}>
                                      {dep.dep_type}
                                    </span>
                                  </td>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      </div>
                    </Show>
                  </Show>

                  <Show when={(dependents() ?? []).length > 0}>
                    <div class="section-head" style={{ 'margin-top': '20px' }}>
                      <span class="section-title">Used by</span>
                      <span class="dim small">{dependents()!.length} package(s) in this registry</span>
                    </div>
                    <div class="grid-cards">
                      <For each={dependents()}>
                        {(dep) => (
                          <div class="card card-pad row">
                            <Icon name="package" size={15} class="icon dim" />
                            <span class="mono grow truncate" style={{ color: 'var(--ink)' }}>
                              {dep.name}
                            </span>
                            <span class="version">{dep.version}</span>
                          </div>
                        )}
                      </For>
                    </div>
                  </Show>
                </Show>

                {/* Security */}
                <Show when={activeTab() === 'security'}>
                  <div class="section-head">
                    <div class="row">
                      <span class="section-title">Vulnerability scan</span>
                      <Show when={vulnData()}>
                        {(vd) => (
                          <span class={`chip ${vd().vulnerabilities.length === 0 ? 'chip-ok' : 'chip-danger'}`}>
                            {vd().vulnerabilities.length === 0
                              ? 'clean'
                              : `${vd().vulnerabilities.length} finding(s)`}
                          </span>
                        )}
                      </Show>
                    </div>
                    <div class="row">
                      <Show when={vulnData()?.scanned_at}>
                        <span class="dim small nowrap">scanned {timeAgo(vulnData()!.scanned_at)}</span>
                      </Show>
                      <button class="btn btn-ghost btn-sm" onClick={() => loadVulns(true)} disabled={vulnLoading()}>
                        <Icon name="refresh" size={13} />
                        Rescan
                      </button>
                    </div>
                  </div>

                  <Show when={vulnError()}>
                    <div class="alert alert-warn">
                      <Icon name="alert-triangle" size={15} />
                      <span>{vulnError()}</span>
                    </div>
                  </Show>

                  <Show when={vulnLoading()}>
                    <TableSkeleton rows={3} cols={4} />
                  </Show>

                  <Show when={!vulnLoading() && vulnData()}>
                    {(vd) => (
                      <Show
                        when={vd().vulnerabilities.length > 0}
                        fallback={
                          <div class="card">
                            <EmptyState
                              icon="shield-check"
                              title="No known vulnerabilities"
                              text={`OSV.dev has no advisories for ${d().name}@${vd().version}.`}
                            />
                          </div>
                        }
                      >
                        <div class="table-card">
                          <div class="table-scroll">
                            <table class="table">
                              <thead>
                                <tr>
                                  <th>Advisory</th>
                                  <th>Severity</th>
                                  <th>Title</th>
                                  <th>Fixed in</th>
                                </tr>
                              </thead>
                              <tbody>
                                <For each={vd().vulnerabilities}>
                                  {(vuln) => (
                                    <tr>
                                      <td class="cell-mono">{vuln.id}</td>
                                      <td>
                                        <span class={`chip ${SEVERITY_CHIP[vuln.severity.toLowerCase()] ?? 'chip-neutral'}`}>
                                          {vuln.severity}
                                        </span>
                                      </td>
                                      <td class="cell-muted">{vuln.title}</td>
                                      <td class="cell-mono cell-muted">{vuln.fixed_in || '—'}</td>
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
                </Show>
              </div>

              {/* Side column */}
              <div class="side-stack">
                <div class="card card-pad">
                  <div class="side-label">Total downloads</div>
                  <div class="side-value">{formatNumber(d().total_downloads)}</div>
                  <div class="divider" />
                  <div class="side-row">
                    <span class="dim">Latest</span>
                    <span class="version">{latestVersion()}</span>
                  </div>
                  <div class="side-row">
                    <span class="dim">Versions</span>
                    <span class="cell-mono">{d().versions.length}</span>
                  </div>
                  <div class="side-row">
                    <span class="dim">License</span>
                    <span class="cell-mono">{d().license || '—'}</span>
                  </div>
                </div>

                <Show when={d().dist_tags.length > 0}>
                  <div class="card card-pad">
                    <div class="side-label" style={{ 'margin-bottom': '8px' }}>
                      Distribution tags
                    </div>
                    <For each={d().dist_tags}>
                      {(dt) => (
                        <div class="side-row">
                          <span class="chip chip-accent">{dt.tag}</span>
                          <span class="cell-mono">{dt.version}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>

                <Show when={!session.isAuthenticated()}>
                  <div class="alert alert-info" style={{ margin: 0 }}>
                    <Icon name="info" size={15} />
                    <span>Sign in to promote versions or manage this package.</span>
                  </div>
                </Show>
              </div>
            </div>

            {/* Promote modal */}
            <Modal
              open={showPromote()}
              title="Promote a version"
              subtitle={`${d().name}@${latestVersion()} — the tarball is shared, not copied; lockfiles stay valid.`}
              onClose={() => setShowPromote(false)}
              actions={
                <>
                  <button class="btn btn-ghost" onClick={() => setShowPromote(false)}>
                    Cancel
                  </button>
                  <button
                    class="btn btn-primary"
                    onClick={handlePromote}
                    disabled={promoteLoading() || !promoteFrom() || !promoteTo() || promoteFrom() === promoteTo()}
                  >
                    {promoteLoading() ? 'Promoting…' : 'Promote'}
                  </button>
                </>
              }
            >
              <div class="field">
                <label class="field-label">From repository</label>
                <select class="select" value={promoteFrom()} onChange={(e) => setPromoteFrom(e.currentTarget.value)}>
                  <option value="">Select source…</option>
                  <For each={sourceRepos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
                </select>
              </div>
              <div class="field">
                <label class="field-label">To repository</label>
                <select class="select" value={promoteTo()} onChange={(e) => setPromoteTo(e.currentTarget.value)}>
                  <option value="">Select target…</option>
                  <For each={writableRepos()}>{(r) => <option value={r.name}>{r.name}</option>}</For>
                </select>
                <div class="field-hint">Only hosted repositories where you hold write access are listed.</div>
              </div>
            </Modal>
          </>
        )}
      </Show>
    </div>
  );
}
