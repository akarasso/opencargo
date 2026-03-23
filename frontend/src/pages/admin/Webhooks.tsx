import { Show } from 'solid-js';

export default function Webhooks() {
  return (
    <>
      {/* Header Section -- matches Stitch v2-webhooks.html */}
      <header style={{ "margin-bottom": '2rem' }}>
        <div style={{ display: 'flex', "align-items": 'center', "justify-content": 'space-between', "margin-bottom": '0.5rem' }}>
          <span style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "font-weight": '500', color: 'rgba(123, 231, 249, 0.6)', "letter-spacing": '0.3em', "text-transform": 'uppercase' }}>
            Registry Service // Events
          </span>
        </div>
        <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'flex-end' }}>
          <div>
            <h2 style={{ "font-size": '2.5rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.05em', color: 'var(--clr-on-surface)', "margin-bottom": '0.25rem' }}>Webhooks</h2>
            <p style={{ color: 'var(--clr-on-surface-variant)', "font-size": '0.875rem', "max-width": '42rem', "line-height": '1.6' }}>
              Configure HTTP endpoints to receive real-time notifications when specific events occur within your OpenCargo registry.
              These webhooks enable seamless integration with CI/CD pipelines and external monitoring systems.
            </p>
          </div>
        </div>
      </header>

      <div style={{ display: 'grid', "grid-template-columns": '1fr 3fr', gap: '2rem' }}>
        {/* Sidebar Stats / Info */}
        <div style={{ display: 'flex', "flex-direction": 'column', gap: '1.5rem' }}>
          {/* Registry Health card */}
          <div class="card" style={{ "border-left": '2px solid rgba(123, 231, 249, 0.1)' }}>
            <div style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "letter-spacing": '0.2em', color: 'var(--clr-outline-variant)', "text-transform": 'uppercase', "margin-bottom": '1rem' }}>
              Registry Health
            </div>
            <div style={{ display: 'flex', "flex-direction": 'column', gap: '1rem' }}>
              <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                <span style={{ "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>Active Nodes</span>
                <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>0x04</span>
              </div>
              <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                <span style={{ "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>Avg Latency</span>
                <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-primary)' }}>24ms</span>
              </div>
              <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center' }}>
                <span style={{ "font-size": '0.75rem', color: 'var(--clr-on-surface-variant)' }}>Event Queue</span>
                <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-mono)', color: 'var(--clr-secondary)' }}>Idle</span>
              </div>
            </div>
          </div>

          {/* Quick Manual */}
          <div style={{ background: 'var(--clr-surface-container-high)', "border-radius": '0.75rem', padding: '1.5rem', "border-top": '1px solid rgba(67, 72, 78, 0.1)' }}>
            <div style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "letter-spacing": '0.2em', color: 'var(--clr-outline-variant)', "text-transform": 'uppercase', "margin-bottom": '1rem' }}>
              Quick Manual
            </div>
            <p style={{ "font-size": '0.6875rem', color: 'var(--clr-on-surface-variant)', "line-height": '1.6' }}>
              Webhooks are configured in your <code style={{ color: 'var(--clr-primary)', "font-family": 'var(--font-mono)' }}>config.toml</code> file. Add a <code style={{ color: 'var(--clr-primary)', "font-family": 'var(--font-mono)' }}>[webhooks.name]</code> section to register endpoints.
            </p>
          </div>
        </div>

        {/* Main Content Area */}
        <div style={{ display: 'flex', "flex-direction": 'column', gap: '2rem' }}>
          {/* Empty State -- matches Stitch v2-webhooks.html */}
          <div class="webhooks-empty-state">
            <div style={{ position: 'absolute', inset: '0', opacity: '0.05', "pointer-events": 'none', background: 'radial-gradient(circle at center, var(--clr-primary), transparent)' }} />
            <div class="webhooks-empty-icon">
              <span class="material-symbols-outlined" style={{ "font-size": '2.5rem', color: 'rgba(123, 231, 249, 0.4)' }}>sensors_off</span>
            </div>
            <h3 style={{ "font-size": '1.25rem', "font-family": 'var(--font-headline)', "font-weight": '700', "margin-bottom": '0.5rem' }}>No webhooks configured.</h3>
            <p style={{ "font-size": '0.875rem', color: 'var(--clr-on-surface-variant)', "max-width": '20rem', "margin-bottom": '2rem' }}>
              Your registry is currently isolated. Connect external systems to start receiving event streams.
            </p>

            {/* Example Config Block -- matches Stitch */}
            <div class="webhooks-config-example">
              <div style={{ display: 'flex', "justify-content": 'space-between', "align-items": 'center', "margin-bottom": '1rem', "padding-bottom": '0.5rem', "border-bottom": '1px solid rgba(67, 72, 78, 0.1)' }}>
                <span style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "letter-spacing": '0.15em', "text-transform": 'uppercase', color: 'var(--clr-outline-variant)' }}>example_config.toml</span>
                <span class="material-symbols-outlined" style={{ "font-size": '14px', color: 'var(--clr-outline-variant)', cursor: 'pointer' }}>content_copy</span>
              </div>
              <div style={{ "font-family": 'var(--font-mono)', "font-size": '0.75rem', display: 'flex', "flex-direction": 'column', gap: '0.25rem' }}>
                <p><span style={{ color: 'var(--clr-secondary)' }}>[webhooks.production]</span></p>
                <p><span style={{ color: 'var(--clr-primary)' }}>url</span> = <span style={{ color: 'var(--clr-on-tertiary-container)' }}>"https://api.internal.sys/hooks"</span></p>
                <p><span style={{ color: 'var(--clr-primary)' }}>events</span> = [<span style={{ color: 'var(--clr-on-tertiary-container)' }}>"package.published"</span>, <span style={{ color: 'var(--clr-on-tertiary-container)' }}>"node.sync"</span>]</p>
                <p><span style={{ color: 'var(--clr-primary)' }}>secret</span> = <span style={{ color: 'var(--clr-on-tertiary-container)' }}>"cargo_sec_0xA4..."</span></p>
                <p><span style={{ color: 'var(--clr-primary)' }}>active</span> = <span style={{ color: 'var(--clr-secondary)' }}>true</span></p>
              </div>
            </div>
          </div>

          {/* Simulation table header -- matches Stitch */}
          <div style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', "letter-spacing": '0.2em', color: 'var(--clr-outline-variant)', "text-transform": 'uppercase' }}>
            Registry Entries (Preview)
          </div>

          {/* Example webhook entries table -- matches Stitch v2-webhooks.html */}
          <div class="data-table-wrapper">
            <table class="data-table">
              <thead>
                <tr>
                  <th>Status</th>
                  <th>Endpoint URL</th>
                  <th>Events</th>
                  <th>Secret</th>
                  <th>Actions</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td colspan="5" style={{ "text-align": 'center', padding: '2rem', color: 'var(--clr-on-surface-variant)' }}>
                    <span style={{ "font-size": '0.75rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.1em' }}>
                      No webhooks registered. Configure webhooks in config.toml to see them here.
                    </span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
      </div>
    </>
  );
}
