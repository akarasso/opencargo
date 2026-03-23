import { createSignal, Show } from 'solid-js';
import { useNavigate } from '@solidjs/router';
import auth from '../lib/auth.ts';

export default function Login() {
  const navigate = useNavigate();
  const [username, setUsername] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);

  if (auth.isAuthenticated()) {
    navigate('/admin', { replace: true });
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    setError(null);
    setLoading(true);

    const err = await auth.login(username(), password());
    setLoading(false);

    if (err) {
      setError(err);
    } else if (auth.mustChangePassword()) {
      navigate('/admin/password', { replace: true });
    } else {
      navigate('/admin', { replace: true });
    }
  }

  return (
    <div class="login-page">
      {/* Top Navigation Anchor (from Stitch) */}
      <header class="login-top-header">
        <div class="login-top-brand">OPENCARGO</div>
        <div class="login-top-meta">
          <span class="material-symbols-outlined" style={{ "font-size": "16px" }}>terminal</span>
          <span style={{ opacity: 0.4, "letter-spacing": "0.3em" }}>SECURE_NODE: 0x44F</span>
        </div>
      </header>

      {/* Main Content Canvas */}
      <main class="login-container">
        {/* Brand Header */}
        <div class="login-header">
          <div class="login-icon">
            <span class="material-symbols-outlined">terminal</span>
          </div>
          <h1 class="login-title">
            <span>OPEN</span><span class="login-title-accent">CARGO</span>
          </h1>
          <p class="login-subtitle">Kinetic Terminal Access</p>
        </div>

        {/* Login Card */}
        <div class="login-card">
          <Show when={error()}>
            <div class="login-error">{error()}</div>
          </Show>

          <form onSubmit={handleSubmit} style={{ display: 'flex', "flex-direction": 'column', gap: '1.5rem' }}>
            {/* Username Field */}
            <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
              <div class="login-field-header">
                <label class="login-label" for="username">Username</label>
                <span class="login-label-hint">REQ_AUTH_ID</span>
              </div>
              <div class="login-input-wrap">
                <input
                  id="username"
                  class="login-input"
                  type="text"
                  value={username()}
                  onInput={(e) => setUsername(e.currentTarget.value)}
                  placeholder="terminal_user_01"
                  autocomplete="username"
                  required
                />
                <div class="login-input-accent" />
              </div>
            </div>

            {/* Password Field */}
            <div style={{ display: 'flex', "flex-direction": 'column', gap: '0.5rem' }}>
              <div class="login-field-header">
                <label class="login-label" for="password">Password</label>
                <span class="login-label-link">Recovery Protocol?</span>
              </div>
              <div class="login-input-wrap">
                <input
                  id="password"
                  class="login-input"
                  type="password"
                  value={password()}
                  onInput={(e) => setPassword(e.currentTarget.value)}
                  placeholder="••••••••"
                  autocomplete="current-password"
                  required
                />
                <div class="login-input-accent" />
              </div>
            </div>

            {/* Session Options */}
            <div class="login-persist">
              <input id="persist" type="checkbox" class="login-checkbox" />
              <label for="persist" class="login-persist-label">Persist Session State</label>
            </div>

            {/* Action */}
            <button
              type="submit"
              class="login-submit"
              disabled={loading()}
            >
              {loading() ? 'Initializing...' : 'Initialize Connection'}
            </button>
          </form>

          {/* Card Footer Metadata */}
          <div class="login-footer">
            <div class="login-status">
              <span class="login-status-led" />
              <span>System Ready</span>
            </div>
            <span>v0.1.0-KINETIC</span>
          </div>
        </div>

        {/* Security Notice */}
        <p class="login-legal">
          Unauthorized access is logged.
        </p>
      </main>

      {/* Global Footer Bar */}
      <footer class="login-bottom-footer">
        <div>
          &copy; 2024 OPENCARGO REGISTRY
        </div>
        <div class="login-bottom-footer-links">
          <a href="#">Privacy Policy</a>
          <a href="#">Terms of Service</a>
          <a href="#">Legal</a>
        </div>
      </footer>

      {/* Background Decoration */}
      <div class="login-bg-decor">
        <div class="login-bg-orb-1" />
        <div class="login-bg-orb-2" />
      </div>
    </div>
  );
}
