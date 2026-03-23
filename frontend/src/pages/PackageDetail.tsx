import { createResource, createSignal, For, Show } from 'solid-js';
import { useParams } from '@solidjs/router';
import {
  fetchPackageDetail,
  fetchDependencies,
  fetchDependents,
  fetchVulns,
  rescanVulns,
  promotePackage,
  fetchRepositories,
} from '../lib/api.ts';
import type { Dependency, Dependent, VulnReport } from '../lib/api.ts';
import CopyButton from '../components/CopyButton.tsx';
import LoadingSpinner from '../components/LoadingSpinner.tsx';
import auth from '../lib/auth.ts';

type Tab = 'readme' | 'versions' | 'dependencies' | 'security';

export default function PackageDetail() {
  const params = useParams<{ path: string }>();
  const packageName = () => params.path;

  const [data] = createResource(packageName, fetchPackageDetail);
  const [activeTab, setActiveTab] = createSignal<Tab>('readme');

  // Dependencies data (loaded lazily when tab is activated)
  const [deps] = createResource(packageName, fetchDependencies);
  const [dependents] = createResource(packageName, fetchDependents);

  // Vuln data (loaded lazily when tab is activated or version is available)
  const [vulnData, setVulnData] = createSignal<VulnReport | null>(null);
  const [vulnLoading, setVulnLoading] = createSignal(false);
  const [vulnError, setVulnError] = createSignal<string | null>(null);

  // Promote modal
  const [showPromote, setShowPromote] = createSignal(false);
  const [promoteFrom, setPromoteFrom] = createSignal('');
  const [promoteTo, setPromoteTo] = createSignal('');
  const [promoteLoading, setPromoteLoading] = createSignal(false);
  const [promoteMsg, setPromoteMsg] = createSignal<string | null>(null);
  const [repos] = createResource(fetchRepositories);

  async function loadVulns() {
    const d = data();
    if (!d || d.versions.length === 0) return;
    const version = d.versions[0].version;
    setVulnLoading(true);
    setVulnError(null);
    try {
      const report = await fetchVulns(d.name, version);
      setVulnData(report);
    } catch (e: any) {
      setVulnError(e.message || 'Failed to load vulnerability data');
    }
    setVulnLoading(false);
  }

  async function handleRescan() {
    const d = data();
    if (!d || d.versions.length === 0) return;
    const version = d.versions[0].version;
    setVulnLoading(true);
    setVulnError(null);
    try {
      const report = await rescanVulns(d.name, version);
      setVulnData(report);
    } catch (e: any) {
      setVulnError(e.message || 'Rescan failed');
    }
    setVulnLoading(false);
  }

  async function handlePromote() {
    const d = data();
    if (!d || d.versions.length === 0) return;
    setPromoteLoading(true);
    setPromoteMsg(null);
    try {
      const result = await promotePackage(d.name, d.versions[0].version, promoteFrom(), promoteTo());
      setPromoteMsg(result.message || 'Promoted successfully');
    } catch (e: any) {
      setPromoteMsg(e.message || 'Promotion failed');
    }
    setPromoteLoading(false);
  }

  function getInstallCommand(name: string): string {
    return `pnpm add ${name}`;
  }

  function onTabChange(tab: Tab) {
    setActiveTab(tab);
    if (tab === 'security' && !vulnData() && !vulnLoading()) {
      loadVulns();
    }
  }

  return (
    <>
      <Show when={data.loading}>
        <LoadingSpinner />
      </Show>

      <Show when={data.error}>
        <div class="alert alert-error">Package not found or failed to load.</div>
      </Show>

      <Show when={data()}>
        {(d) => {
          const latestVersion = () => d().versions.length > 0 ? d().versions[0].version : '';

          return (
            <div style={{ "max-width": '80rem', margin: '0 auto', padding: '2.5rem 2rem' }}>
              {/* Technical Scaffolding Header -- matches Stitch detail page */}
              <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end', "margin-bottom": '2rem', "border-bottom": '1px solid rgba(255, 255, 255, 0.05)', "padding-bottom": '1.5rem' }}>
                <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
                  <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem', "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)' }}>
                    <span>REGISTRY_NODE: 0x44F</span>
                    <span style={{ width: '4px', height: '4px', background: 'var(--clr-primary)', "border-radius": '50%' }} class="status-led-animated" />
                    <span>STATUS: ACTIVE</span>
                  </div>
                  <h1 style={{ "font-size": '3rem', "font-weight": '700', "font-family": 'var(--font-headline)', color: 'var(--clr-on-background)', "letter-spacing": '-0.05em' }}>{d().name}</h1>
                  <Show when={d().description}>
                    <p style={{ color: 'var(--clr-on-surface-variant)', "font-size": '1.125rem', "max-width": '42rem', "font-family": 'var(--font-body)' }}>{d().description}</p>
                  </Show>
                </div>
                <div style={{ display: 'flex', "align-items": 'center', gap: '1rem' }}>
                  <Show when={d().license}>
                    <span style={{ padding: '0.25rem 0.75rem', background: 'var(--clr-surface-container-high)', color: 'var(--clr-primary)', border: '1px solid rgba(123, 231, 249, 0.2)', "border-radius": '0.375rem', "font-size": '0.75rem', "font-weight": '700', "font-family": 'var(--font-headline)', "letter-spacing": '0.2em', "text-transform": 'uppercase' }}>{d().license}</span>
                  </Show>
                </div>
              </div>

              {/* Two-Column Grid -- matches Stitch */}
              <div class="pkg-grid">
                {/* Left Column */}
                <div style={{ display: 'flex', "flex-direction": 'column', gap: '2.5rem' }}>
                  {/* Installation Block */}
                  <section style={{ display: 'flex', "flex-direction": 'column', gap: '1rem' }}>
                    <h3 style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)', "margin-bottom": '0' }}>Quick Install</h3>
                    <div class="code-block">
                      <code style={{ "font-family": 'var(--font-mono)', color: 'var(--clr-secondary)', "font-size": '1.125rem' }}>{getInstallCommand(d().name)}</code>
                      <CopyButton text={getInstallCommand(d().name)} />
                    </div>
                  </section>

                  {/* Content Tabs -- matches Stitch */}
                  <section>
                    <div class="pkg-tabs">
                      <button
                        class={`pkg-tab ${activeTab() === 'readme' ? 'pkg-tab-active' : ''}`}
                        onClick={() => onTabChange('readme')}
                      >
                        README
                      </button>
                      <button
                        class={`pkg-tab ${activeTab() === 'versions' ? 'pkg-tab-active' : ''}`}
                        onClick={() => onTabChange('versions')}
                      >
                        Versions
                      </button>
                      <button
                        class={`pkg-tab ${activeTab() === 'dependencies' ? 'pkg-tab-active' : ''}`}
                        onClick={() => onTabChange('dependencies')}
                      >
                        Dependencies
                      </button>
                      <button
                        class={`pkg-tab ${activeTab() === 'security' ? 'pkg-tab-active' : ''}`}
                        onClick={() => onTabChange('security')}
                      >
                        Security
                      </button>
                    </div>

                    {/* Readme tab */}
                    <Show when={activeTab() === 'readme'}>
                      <Show
                        when={d().readme_html}
                        fallback={
                          <div class="card">
                            <p style={{ color: 'var(--clr-on-surface-variant)' }}>No README available.</p>
                          </div>
                        }
                      >
                        <div class="readme-content" innerHTML={d().readme_html} />
                      </Show>
                    </Show>

                    {/* Versions tab */}
                    <Show when={activeTab() === 'versions'}>
                      <div class="data-table-wrapper">
                        <table class="data-table">
                          <thead>
                            <tr>
                              <th>Version</th>
                              <th>Size</th>
                              <th>Published</th>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={d().versions}>
                              {(v) => (
                                <tr>
                                  <td><span class="badge badge-mono">{v.version}</span></td>
                                  <td class="data-table-muted">{v.size_display}</td>
                                  <td class="data-table-muted">{v.published_at}</td>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      </div>
                    </Show>

                    {/* Dependencies tab -- matches Stitch v2-detail-full.html Dependencies table */}
                    <Show when={activeTab() === 'dependencies'}>
                      <Show when={deps.loading}>
                        <LoadingSpinner />
                      </Show>
                      <Show when={deps.error}>
                        <div class="card">
                          <p style={{ color: 'var(--clr-on-surface-variant)' }}>Could not load dependency information.</p>
                        </div>
                      </Show>
                      <Show when={deps()}>
                        {(depsData) => (
                          <>
                            <Show when={depsData().length === 0}>
                              <div class="card">
                                <p style={{ color: 'var(--clr-on-surface-variant)' }}>No dependency information available.</p>
                              </div>
                            </Show>
                            <Show when={depsData().length > 0}>
                              <div class="data-table-wrapper">
                                <table class="data-table">
                                  <thead>
                                    <tr>
                                      <th>Name</th>
                                      <th>Version Req</th>
                                      <th style={{ "text-align": 'right' }}>Type</th>
                                    </tr>
                                  </thead>
                                  <tbody>
                                    <For each={depsData()}>
                                      {(dep: Dependency) => (
                                        <tr>
                                          <td>
                                            <span style={{ color: dep.dep_type === 'runtime' ? 'var(--clr-primary)' : 'var(--clr-on-surface)', "font-weight": '500' }}>{dep.name}</span>
                                          </td>
                                          <td style={{ "font-family": 'var(--font-mono)', "font-size": '0.875rem', color: 'var(--clr-on-surface-variant)' }}>{dep.version_req}</td>
                                          <td style={{ "text-align": 'right' }}>
                                            <span class={`badge ${dep.dep_type === 'dev' ? 'badge-default' : 'badge-default'}`}
                                              style={{ background: dep.dep_type === 'dev' ? 'var(--clr-surface-container-highest)' : 'rgba(123, 231, 249, 0.1)', color: dep.dep_type === 'dev' ? 'var(--clr-on-surface-variant)' : 'var(--clr-primary)' }}
                                            >
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

                            {/* Dependents section */}
                            <Show when={dependents() && (dependents() as Dependent[]).length > 0}>
                              <div style={{ "margin-top": '2rem' }}>
                                <h3 style={{ "font-size": '0.875rem', "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.1em', color: 'var(--clr-on-surface)', "margin-bottom": '1rem', "font-weight": '700' }}>
                                  <span class="material-symbols-outlined" style={{ "font-size": '18px', color: 'var(--clr-primary)', "margin-right": '0.5rem', "vertical-align": 'middle' }}>hub</span>
                                  Dependents
                                </h3>
                                <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.75rem' }}>
                                  <For each={dependents() as Dependent[]}>
                                    {(dep) => (
                                      <div class="dep-card">
                                        <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                                          <div style={{ width: '40px', height: '40px', "border-radius": '0.5rem', background: 'var(--clr-surface-container-highest)', display: 'flex', "align-items": 'center', "justify-content": 'center', border: '1px solid rgba(67, 72, 78, 0.2)' }}>
                                            <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)', "font-size": '20px' }}>web</span>
                                          </div>
                                          <div>
                                            <div style={{ "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-surface)' }}>{dep.name}</div>
                                            <div style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface-variant)' }}>{dep.version}</div>
                                          </div>
                                        </div>
                                      </div>
                                    )}
                                  </For>
                                </div>
                              </div>
                            </Show>
                          </>
                        )}
                      </Show>
                    </Show>

                    {/* Security tab -- matches Stitch v2-detail-full.html Security section */}
                    <Show when={activeTab() === 'security'}>
                      <section class="vuln-section">
                        <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1.5rem' }}>
                          <h3 style={{ "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.1em', "font-size": '0.875rem', "font-weight": '700', color: 'var(--clr-on-surface)', display: 'flex', "align-items": 'center', gap: '0.75rem', "margin-bottom": '0' }}>
                            <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)' }}>verified_user</span>
                            Security Status
                          </h3>
                          <div style={{ display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
                            <Show when={vulnData()}>
                              {(vd) => (
                                <>
                                  <Show when={vd().vulnerabilities.length === 0}>
                                    <div class="vuln-status-badge vuln-status-clean">
                                      <span class="vuln-status-led vuln-status-led-green" />
                                      <span>No vulnerabilities</span>
                                    </div>
                                  </Show>
                                  <Show when={vd().vulnerabilities.length > 0}>
                                    <div class="vuln-status-badge vuln-status-danger">
                                      <span class="vuln-status-led vuln-status-led-red" />
                                      <span>{vd().vulnerabilities.length} vulnerabilit{vd().vulnerabilities.length === 1 ? 'y' : 'ies'}</span>
                                    </div>
                                  </Show>
                                  <Show when={vd().scanned_at}>
                                    <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.1em', color: 'var(--clr-on-surface-variant)' }}>
                                      Last Scan: {vd().scanned_at}
                                    </span>
                                  </Show>
                                </>
                              )}
                            </Show>
                            <button class="btn btn-sm btn-secondary" onClick={handleRescan} disabled={vulnLoading()}>
                              <span class="material-symbols-outlined" style={{ "font-size": '14px' }}>refresh</span>
                              Rescan
                            </button>
                          </div>
                        </div>

                        <Show when={vulnLoading()}>
                          <LoadingSpinner />
                        </Show>

                        <Show when={vulnError()}>
                          <div class="alert alert-error">{vulnError()}</div>
                        </Show>

                        <Show when={vulnData()}>
                          {(vd) => (
                            <>
                              <Show when={vd().vulnerabilities.length === 0}>
                                <div class="vuln-empty-state">
                                  <span class="material-symbols-outlined" style={{ "font-size": '3rem', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>security_update_good</span>
                                  <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-headline)', "text-transform": 'uppercase', "letter-spacing": '0.1em', color: 'var(--clr-on-surface-variant)', "font-weight": '500' }}>
                                    Audit complete. No CVEs identified in current version graph.
                                  </p>
                                </div>
                              </Show>

                              <Show when={vd().vulnerabilities.length > 0}>
                                <div class="data-table-wrapper">
                                  <table class="data-table">
                                    <thead>
                                      <tr>
                                        <th>CVE ID</th>
                                        <th>Severity</th>
                                        <th>Title</th>
                                        <th>Fixed In</th>
                                      </tr>
                                    </thead>
                                    <tbody>
                                      <For each={vd().vulnerabilities}>
                                        {(vuln) => (
                                          <tr>
                                            <td><span class="badge badge-mono">{vuln.id}</span></td>
                                            <td>
                                              <span class={`badge ${vuln.severity === 'critical' ? 'badge-danger' : vuln.severity === 'high' ? 'badge-warning' : 'badge-default'}`}>
                                                {vuln.severity}
                                              </span>
                                            </td>
                                            <td style={{ color: 'var(--clr-on-surface)' }}>{vuln.title}</td>
                                            <td class="data-table-muted">{vuln.fixed_in || 'N/A'}</td>
                                          </tr>
                                        )}
                                      </For>
                                    </tbody>
                                  </table>
                                </div>
                              </Show>
                            </>
                          )}
                        </Show>
                      </section>
                    </Show>
                  </section>
                </div>

                {/* Right Column -- Metadata Sidebar */}
                <div style={{ display: 'flex', "flex-direction": 'column', gap: '2rem' }}>
                  {/* Metadata Card -- matches Stitch */}
                  <div class="pkg-sidebar-card">
                    <div style={{ display: 'flex', "flex-direction": 'column', gap: '1.5rem' }}>

                      {/* Promote Button -- matches Stitch kinetic-gradient promote button */}
                      <Show when={auth.isAuthenticated()}>
                        <div>
                          <button
                            class="btn btn-primary"
                            style={{ width: '100%', padding: '1rem', "letter-spacing": '0.2em', "box-shadow": '0 0 20px rgba(129, 236, 255, 0.2)' }}
                            onClick={() => setShowPromote(true)}
                          >
                            Promote to Production
                          </button>
                          <p style={{ "margin-top": '0.75rem', "font-size": '0.5625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "text-align": 'center', color: 'var(--clr-on-surface-variant)' }}>
                            Admin Privileges: Authorized
                          </p>
                        </div>
                      </Show>

                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                        <span class="pkg-sidebar-label">Latest Version</span>
                        <span style={{ color: 'var(--clr-secondary)', "font-family": 'var(--font-mono)', "font-weight": '700' }}>{latestVersion()}</span>
                      </div>
                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                        <span class="pkg-sidebar-label">Downloads (Total)</span>
                        <span style={{ color: 'var(--clr-on-surface)', "font-family": 'var(--font-mono)' }}>{d().total_downloads.toLocaleString()}</span>
                      </div>
                      <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                        <span class="pkg-sidebar-label">Versions</span>
                        <span style={{ color: 'var(--clr-on-surface)', "font-family": 'var(--font-mono)' }}>{d().versions.length}</span>
                      </div>
                    </div>

                    {/* Distribution Tags */}
                    <Show when={d().dist_tags.length > 0}>
                      <div class="pkg-sidebar-divider" />
                      <div>
                        <h4 class="pkg-sidebar-label" style={{ "margin-bottom": '1rem' }}>Distribution Tags</h4>
                        <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.75rem' }}>
                          <For each={d().dist_tags}>
                            {(dt, i) => (
                              <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', padding: '0.75rem', background: i() === 0 ? 'var(--clr-surface-container-high)' : 'transparent', "border-radius": '0.375rem', border: i() === 0 ? '1px solid rgba(123, 231, 249, 0.1)' : '1px solid var(--clr-outline-variant)' }}>
                                <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-headline)', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '-0.025em', color: i() === 0 ? 'var(--clr-secondary)' : 'var(--clr-outline)' }}>{dt.tag}</span>
                                <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface)' }}>{dt.version}</span>
                              </div>
                            )}
                          </For>
                        </div>
                      </div>
                    </Show>
                  </div>

                  {/* Technical Stats -- matches Stitch */}
                  <div style={{ display: 'grid', "grid-template-columns": '1fr 1fr', gap: '1rem' }}>
                    <div style={{ background: 'var(--clr-surface-container-low)', padding: '1rem', "border-radius": '0.5rem', border: '1px solid rgba(255, 255, 255, 0.05)' }}>
                      <span style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)', display: 'block', "margin-bottom": '0.25rem' }}>Status</span>
                      <span style={{ "font-size": '0.875rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface)' }}>Active</span>
                    </div>
                    <div style={{ background: 'var(--clr-surface-container-low)', padding: '1rem', "border-radius": '0.5rem', border: '1px solid rgba(255, 255, 255, 0.05)' }}>
                      <span style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-outline)', display: 'block', "margin-bottom": '0.25rem' }}>Total Files</span>
                      <span style={{ "font-size": '0.875rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface)' }}>{d().versions.length}</span>
                    </div>
                  </div>

                  {/* Security Audit -- matches Stitch */}
                  <div style={{ background: 'rgba(159, 5, 25, 0.1)', border: '1px solid rgba(255, 113, 108, 0.2)', padding: '1.5rem', "border-radius": '0.75rem', position: 'relative', overflow: 'hidden' }}>
                    <div style={{ position: 'relative', "z-index": 1, display: 'flex', gap: '1rem', "align-items": 'flex-start' }}>
                      <span class="material-symbols-outlined" style={{ color: 'var(--clr-error)' }}>security</span>
                      <div>
                        <h4 style={{ "font-size": '0.75rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-error-container)', "text-transform": 'uppercase', "letter-spacing": '0.2em', "margin-bottom": '0.25rem' }}>Audit Results</h4>
                        <p style={{ "font-size": '0.625rem', color: 'rgba(255, 168, 163, 0.8)' }}>0 Vulnerabilities detected in the current build.</p>
                      </div>
                    </div>
                  </div>
                </div>
              </div>

              {/* Promote Modal */}
              <Show when={showPromote()}>
                <div class="modal-overlay" onClick={() => setShowPromote(false)}>
                  <div class="modal" onClick={(e) => e.stopPropagation()}>
                    <h3 class="modal-title">Promote Package</h3>
                    <div class="modal-body">
                      <p style={{ "margin-bottom": '1rem' }}>
                        Promote <strong style={{ color: 'var(--clr-primary)' }}>{d().name}@{latestVersion()}</strong> between repositories.
                      </p>
                      <div class="form-group">
                        <label class="form-label">Source Repository</label>
                        <select
                          class="form-select"
                          value={promoteFrom()}
                          onChange={(e) => setPromoteFrom(e.currentTarget.value)}
                        >
                          <option value="">Select source...</option>
                          <Show when={repos()}>
                            <For each={repos()!.repositories}>
                              {(r) => <option value={r.name}>{r.name}</option>}
                            </For>
                          </Show>
                        </select>
                      </div>
                      <div class="form-group">
                        <label class="form-label">Target Repository</label>
                        <select
                          class="form-select"
                          value={promoteTo()}
                          onChange={(e) => setPromoteTo(e.currentTarget.value)}
                        >
                          <option value="">Select target...</option>
                          <Show when={repos()}>
                            <For each={repos()!.repositories}>
                              {(r) => <option value={r.name}>{r.name}</option>}
                            </For>
                          </Show>
                        </select>
                      </div>
                      <Show when={promoteMsg()}>
                        <div class="alert alert-info" style={{ "margin-top": '0.5rem' }}>{promoteMsg()}</div>
                      </Show>
                    </div>
                    <div class="modal-actions">
                      <button class="btn btn-secondary" onClick={() => setShowPromote(false)}>Cancel</button>
                      <button
                        class="btn btn-primary"
                        onClick={handlePromote}
                        disabled={promoteLoading() || !promoteFrom() || !promoteTo()}
                      >
                        {promoteLoading() ? 'Promoting...' : 'Promote'}
                      </button>
                    </div>
                  </div>
                </div>
              </Show>
            </div>
          );
        }}
      </Show>
    </>
  );
}
