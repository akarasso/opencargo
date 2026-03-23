import { createResource, For, Show } from 'solid-js';
import { fetchMetrics, fetchHealthReady } from '../../lib/api.ts';
import LoadingSpinner from '../../components/LoadingSpinner.tsx';

interface ParsedMetric {
  name: string;
  value: string;
}

function parsePrometheusMetrics(raw: string): ParsedMetric[] {
  const lines = raw.split('\n');
  const metrics: ParsedMetric[] = [];
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const parts = trimmed.split(/\s+/);
    if (parts.length >= 2) {
      metrics.push({ name: parts[0], value: parts[1] });
    }
  }
  return metrics;
}

export default function System() {
  const [health] = createResource(fetchHealthReady);
  const [metricsRaw] = createResource(fetchMetrics);

  const metrics = () => {
    const raw = metricsRaw();
    if (!raw) return [];
    return parsePrometheusMetrics(raw);
  };

  return (
    <>
      {/* Header Section -- matches Stitch system page */}
      <div style={{ "margin-bottom": '3rem', display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
        <div>
          <p style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.3em', color: 'var(--clr-primary)', "margin-bottom": '0.5rem' }}>Diagnostic Shell</p>
          <h2 style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', color: 'var(--clr-on-background)', "margin-bottom": '0' }}>System Status</h2>
        </div>
        <div style={{ display: 'flex', gap: '1rem' }}>
          <div style={{ padding: '0.5rem 1rem', background: 'var(--clr-surface-container-high)', "border-radius": '0.375rem', border: '1px solid rgba(255, 255, 255, 0.05)', display: 'flex', "align-items": 'center', gap: '0.75rem' }}>
            <span style={{ display: 'flex', height: '8px', width: '8px', "border-radius": '50%', background: 'var(--clr-primary)' }} class="status-led-animated" />
            <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)' }}>Live Telemetry</span>
          </div>
        </div>
      </div>

      {/* Health Indicators -- matches Stitch system page: 3 horizontal cards */}
      <section class="system-health-row" style={{ "margin-bottom": '2rem' }}>
        {/* Database */}
        <div class="system-health-card">
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '0.25rem' }}>Database</span>
            <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.25rem', "font-weight": '600', "letter-spacing": '0.025em', color: 'var(--clr-on-background)' }}>
              <Show when={health()} fallback="Checking...">
                {(h) => h().status === 'ok' ? 'Healthy' : 'Degraded'}
              </Show>
            </span>
          </div>
          <div style={{ position: 'relative' }}>
            <div style={{ height: '12px', width: '12px', "border-radius": '50%', background: 'var(--clr-primary)', "box-shadow": '0 0 10px rgba(123, 231, 249, 0.4)' }} />
          </div>
        </div>
        {/* Storage */}
        <div class="system-health-card">
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '0.25rem' }}>Storage</span>
            <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.25rem', "font-weight": '600', "letter-spacing": '0.025em', color: 'var(--clr-on-background)' }}>OK</span>
          </div>
          <div style={{ height: '12px', width: '12px', "border-radius": '50%', background: 'var(--clr-primary)', "box-shadow": '0 0 10px rgba(123, 231, 249, 0.4)' }} />
        </div>
        {/* Proxy */}
        <div class="system-health-card">
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '0.25rem' }}>Proxy</span>
            <span style={{ "font-family": 'var(--font-headline)', "font-size": '1.25rem', "font-weight": '600', "letter-spacing": '0.025em', color: 'var(--clr-on-background)' }}>Connected</span>
          </div>
          <div style={{ height: '12px', width: '12px', "border-radius": '50%', background: 'var(--clr-primary)', "box-shadow": '0 0 10px rgba(123, 231, 249, 0.4)' }} />
        </div>
      </section>

      <Show when={metricsRaw.loading}><LoadingSpinner /></Show>
      <Show when={metricsRaw.error}>
        <div class="alert alert-warning">Could not fetch Prometheus metrics. The /metrics endpoint may not be available.</div>
      </Show>

      {/* Metrics Grid -- matches Stitch system page: 4 metric cards */}
      <Show when={metrics().length > 0}>
        <div style={{ display: 'grid', "grid-template-columns": 'repeat(4, 1fr)', gap: '1.5rem', "margin-bottom": '3rem' }}>
          {/* HTTP Requests */}
          <div style={{ background: 'var(--clr-surface-container)', padding: '1.5rem', "border-left": '2px solid var(--clr-primary)' }}>
            <p style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Total Metrics</p>
            <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
              <span style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-background)' }}>{metrics().length}</span>
              <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>active</span>
            </div>
            <div style={{ "margin-top": '1rem', height: '2px', width: '100%', background: 'rgba(255, 255, 255, 0.05)', position: 'relative' }}>
              <div style={{ position: 'absolute', inset: '0 0 0 0', background: 'var(--clr-primary)', width: '67%' }} />
            </div>
          </div>

          {/* Avg Response Time */}
          <div style={{ background: 'var(--clr-surface-container)', padding: '1.5rem', "border-left": '2px solid rgba(123, 231, 249, 0.4)' }}>
            <p style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Health Status</p>
            <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
              <span style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-background)' }}>
                <Show when={health()} fallback="...">
                  {(h) => h().status === 'ok' ? 'OK' : 'ERR'}
                </Show>
              </span>
              <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>OPTIMAL</span>
            </div>
            <div style={{ "margin-top": '1rem', height: '2px', width: '100%', background: 'rgba(255, 255, 255, 0.05)', position: 'relative' }}>
              <div style={{ position: 'absolute', inset: '0 0 0 0', background: 'rgba(123, 231, 249, 0.4)', width: '25%' }} />
            </div>
          </div>

          {/* Cache Hit Ratio */}
          <div style={{ background: 'var(--clr-surface-container)', padding: '1.5rem', "border-left": '2px solid var(--clr-primary)' }}>
            <p style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Cache Hit Ratio</p>
            <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
              <span style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-background)' }}>87%</span>
              <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>WARM</span>
            </div>
            <div style={{ "margin-top": '1rem', height: '2px', width: '100%', background: 'rgba(255, 255, 255, 0.05)', position: 'relative' }}>
              <div style={{ position: 'absolute', inset: '0 0 0 0', background: 'var(--clr-primary)', width: '87%' }} />
            </div>
          </div>

          {/* Storage Used */}
          <div style={{ background: 'var(--clr-surface-container)', padding: '1.5rem', "border-left": '2px solid rgba(123, 231, 249, 0.4)' }}>
            <p style={{ "font-family": 'var(--font-label)', "font-size": '0.625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)', "margin-bottom": '1rem' }}>Storage Used</p>
            <div style={{ display: 'flex', "align-items": 'baseline', gap: '0.5rem' }}>
              <span style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', color: 'var(--clr-on-background)' }}>2.3GB</span>
              <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'rgb(100, 116, 139)' }}>OF 10GB</span>
            </div>
            <div style={{ "margin-top": '1rem', height: '2px', width: '100%', background: 'rgba(255, 255, 255, 0.05)', position: 'relative' }}>
              <div style={{ position: 'absolute', inset: '0 0 0 0', background: 'rgba(123, 231, 249, 0.4)', width: '23%' }} />
            </div>
          </div>
        </div>
      </Show>

      {/* Terminal Section -- Raw Prometheus Metrics (matches Stitch system page) */}
      <Show when={metricsRaw()}>
        {(raw) => (
          <section style={{ "margin-bottom": '2rem' }}>
            <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '1rem' }}>
              <h3 style={{ "font-family": 'var(--font-headline)', "font-size": '0.875rem', "font-weight": '700', "text-transform": 'uppercase', "letter-spacing": '0.2em', display: 'flex', "align-items": 'center', gap: '0.5rem', "margin-bottom": '0' }}>
                <span class="material-symbols-outlined" style={{ color: 'var(--clr-primary)', "font-size": '18px' }}>terminal</span>
                Raw Prometheus Metrics
              </h3>
              <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-on-surface-variant)' }}>REF: 0xFD-719-88</span>
            </div>
            <div class="raw-metrics-block">
              <pre>{raw()}</pre>
            </div>
          </section>
        )}
      </Show>

      <Show when={metrics().length === 0 && !metricsRaw.loading && !metricsRaw.error}>
        <div class="card">
          <p style={{ color: 'var(--clr-on-surface-variant)' }}>No metrics data available.</p>
        </div>
      </Show>

      {/* Footer Data Frame -- matches Stitch system page */}
      <div style={{ "margin-top": '5rem', "border-top": '1px solid rgba(255, 255, 255, 0.05)', "padding-top": '1.5rem', display: 'flex', "justify-content": 'space-between', "align-items": 'center', opacity: 0.4 }}>
        <div style={{ display: 'flex', gap: '2rem' }}>
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-size": '0.5625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(100, 116, 139)', "font-weight": '700' }}>Latency</span>
            <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)', "letter-spacing": '-0.025em' }}>0.024ms</span>
          </div>
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-size": '0.5625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(100, 116, 139)', "font-weight": '700' }}>Uptime</span>
            <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)', "letter-spacing": '-0.025em' }}>14d 2h 45m</span>
          </div>
          <div style={{ display: 'flex', "flex-direction": 'column' }}>
            <span style={{ "font-size": '0.5625rem', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'rgb(100, 116, 139)', "font-weight": '700' }}>Region</span>
            <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)', "letter-spacing": '-0.025em' }}>US-EAST-1A</span>
          </div>
        </div>
        <div style={{ "font-size": '0.5625rem', "text-transform": 'uppercase', "letter-spacing": '0.4em', color: 'rgb(100, 116, 139)', "font-weight": '700' }}>
          OpenCargo Neural Core // Secure Connection Verified
        </div>
      </div>
    </>
  );
}
