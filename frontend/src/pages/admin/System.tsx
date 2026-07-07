import { For, Show, createSignal } from 'solid-js';
import Icon from '../../components/Icon.tsx';
import { RequireAdmin } from '../../components/guards.tsx';
import { LoadError, TableSkeleton } from '../../components/bits.tsx';
import { fetchHealthReady, fetchMetrics } from '../../core/api.ts';
import { createLiveResource } from '../../core/stores/live.ts';

interface ParsedMetric {
  name: string;
  labels: string;
  value: string;
}

function parsePrometheusMetrics(raw: string): ParsedMetric[] {
  const metrics: ParsedMetric[] = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const space = trimmed.lastIndexOf(' ');
    if (space === -1) continue;
    const key = trimmed.slice(0, space);
    const value = trimmed.slice(space + 1);
    const brace = key.indexOf('{');
    metrics.push({
      name: brace === -1 ? key : key.slice(0, brace),
      labels: brace === -1 ? '' : key.slice(brace),
      value,
    });
  }
  return metrics;
}

export default function System() {
  return (
    <RequireAdmin>
      <SystemInner />
    </RequireAdmin>
  );
}

function SystemInner() {
  const [health, refetchHealth] = createLiveResource(fetchHealthReady, [], { debounce: 500 });
  const [metricsRaw, refetchMetrics] = createLiveResource(fetchMetrics, [], { debounce: 500 });
  const [filter, setFilter] = createSignal('');

  const metrics = () => {
    const raw = metricsRaw();
    if (!raw) return [];
    const q = filter().toLowerCase();
    const all = parsePrometheusMetrics(raw);
    return q ? all.filter((m) => m.name.toLowerCase().includes(q) || m.labels.toLowerCase().includes(q)) : all;
  };

  return (
    <div class="page-enter">
      <div class="page-head">
        <div>
          <h1 class="page-title">System</h1>
          <p class="page-sub">
            Health checks and the raw Prometheus counters this instance exposes at{' '}
            <span class="mono">/metrics</span>.
          </p>
        </div>
        <div class="page-actions">
          <button
            class="btn btn-ghost"
            onClick={() => {
              void refetchHealth();
              void refetchMetrics();
            }}
          >
            <Icon name="refresh" size={14} />
            Refresh
          </button>
        </div>
      </div>

      <div class="stagger">
        <section class="stats-grid section" style={{ 'grid-template-columns': 'repeat(2, minmax(0, 1fr))' }}>
          <div class="stat" style={{ '--stat-tint': health()?.status === 'ok' ? 'var(--ok)' : 'var(--danger)' }}>
            <div class="stat-head">
              <span class="stat-label">Database</span>
              <Icon name="database" size={16} />
            </div>
            <div class="stat-value" style={{ 'font-size': '1.3rem' }}>
              <Show when={health()} fallback={<span class="dim">checking…</span>}>
                {(h) => <>{h().status === 'ok' ? 'Healthy' : 'Degraded'}</>}
              </Show>
            </div>
            <div class="stat-foot">SQLite · embedded</div>
          </div>
          <div class="stat" style={{ '--stat-tint': 'var(--steel)' }}>
            <div class="stat-head">
              <span class="stat-label">Metrics exported</span>
              <Icon name="activity" size={16} />
            </div>
            <div class="stat-value" style={{ 'font-size': '1.3rem' }}>
              {metricsRaw() ? parsePrometheusMetrics(metricsRaw()!).length : '—'}
            </div>
            <div class="stat-foot">Prometheus text format</div>
          </div>
        </section>

        <Show when={metricsRaw.error}>
          <LoadError what="metrics" />
        </Show>

        <section class="section">
          <div class="filter-bar">
            <div class="search-box">
              <Icon name="search" size={15} />
              <input
                class="input"
                placeholder="Filter metrics… (http_requests, opencargo_)"
                value={filter()}
                onInput={(e) => setFilter(e.currentTarget.value)}
                spellcheck={false}
              />
            </div>
          </div>

          <Show when={metricsRaw()} fallback={<TableSkeleton rows={8} cols={2} />}>
            <div class="table-card">
              <div class="table-scroll" style={{ 'max-height': '520px', 'overflow-y': 'auto' }}>
                <table class="table">
                  <thead>
                    <tr>
                      <th>Metric</th>
                      <th style={{ 'text-align': 'right' }}>Value</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={metrics().slice(0, 400)}>
                      {(m) => (
                        <tr>
                          <td>
                            <span class="cell-mono" style={{ color: 'var(--ink)' }}>
                              {m.name}
                            </span>
                            <Show when={m.labels}>
                              <div class="cell-mono cell-dim truncate" style={{ 'max-width': '460px', 'font-size': '0.68rem' }}>
                                {m.labels}
                              </div>
                            </Show>
                          </td>
                          <td class="cell-mono cell-num" style={{ 'text-align': 'right', color: 'var(--accent)' }}>
                            {m.value}
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </div>
          </Show>
        </section>
      </div>
    </div>
  );
}
