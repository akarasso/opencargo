import { Show, createEffect, createSignal } from 'solid-js';
import { A, useNavigate } from '@solidjs/router';
import Icon from '../components/Icon.tsx';
import { session } from '../core/stores/session.ts';
import { toasts } from '../core/stores/toasts.ts';

export default function Login() {
  const navigate = useNavigate();
  const [username, setUsername] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [showPassword, setShowPassword] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);

  // Already signed in (or becomes signed in): leave the login page.
  createEffect(() => {
    if (!session.checking() && session.isAuthenticated() && !loading()) {
      const u = session.user();
      navigate(u?.mustChangePassword ? '/admin/password' : '/', { replace: true });
    }
  });

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (loading()) return;
    setError(null);
    setLoading(true);

    const err = await session.login(username().trim(), password());
    setLoading(false);

    if (err) {
      setError(err);
      return;
    }

    const u = session.user();
    if (u?.mustChangePassword) {
      toasts.info('Welcome back', 'Set a new password to continue.');
      navigate('/admin/password', { replace: true });
    } else {
      toasts.success(`Signed in as ${u?.username}`);
      navigate('/', { replace: true });
    }
  }

  return (
    <div class="login-page">
      <div class="login-grid-decor" aria-hidden="true" />

      <div class="login-box">
        <div class="login-brand">
          <div class="brand-mark">
            <Icon name="anchor" size={22} strokeWidth={2} />
          </div>
          <div>
            <div class="login-title">OpenCargo</div>
            <div class="login-sub">sign in to your registry</div>
          </div>
        </div>

        <form class="login-card" onSubmit={handleSubmit}>
          <Show when={error()}>
            <div class="alert alert-error" role="alert">
              <Icon name="alert-circle" size={15} />
              <span>{error()}</span>
            </div>
          </Show>

          <div class="field">
            <label class="field-label" for="login-username">
              Username
            </label>
            <input
              id="login-username"
              class="input"
              value={username()}
              onInput={(e) => setUsername(e.currentTarget.value)}
              autocomplete="username"
              spellcheck={false}
              required
              autofocus
            />
          </div>

          <div class="field">
            <label class="field-label" for="login-password">
              Password
            </label>
            <div class="search-box">
              <input
                id="login-password"
                class="input"
                style={{ 'padding-left': '11px', 'padding-right': '38px' }}
                type={showPassword() ? 'text' : 'password'}
                value={password()}
                onInput={(e) => setPassword(e.currentTarget.value)}
                autocomplete="current-password"
                required
              />
              <button
                type="button"
                class="btn btn-quiet btn-icon"
                style={{ position: 'absolute', right: '4px' }}
                onClick={() => setShowPassword((v) => !v)}
                aria-label={showPassword() ? 'Hide password' : 'Show password'}
                tabindex={-1}
              >
                <Icon name={showPassword() ? 'eye-off' : 'eye'} size={15} />
              </button>
            </div>
          </div>

          <button class="btn btn-primary" style={{ width: '100%', 'margin-top': '6px' }} disabled={loading()}>
            <Show when={loading()} fallback={<Icon name="log-in" size={15} />}>
              <span class="spinner" style={{ 'border-top-color': 'var(--accent-ink)' }} />
            </Show>
            {loading() ? 'Signing in…' : 'Sign in'}
          </button>

          <div class="field-hint" style={{ 'margin-top': '12px', 'text-align': 'center' }}>
            First run? The admin password is in <span class="mono">data/admin.password</span>.
          </div>
        </form>

        <div class="login-foot">
          <A href="/" style={{ color: 'inherit' }}>
            ← back to registry
          </A>
          <span>npm · cargo · oci · go</span>
        </div>
      </div>
    </div>
  );
}
