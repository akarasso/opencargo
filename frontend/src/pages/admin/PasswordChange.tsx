import { createSignal, Show } from 'solid-js';
import { changePassword } from '../../lib/api.ts';
import auth from '../../lib/auth.ts';

export default function PasswordChange() {
  const [currentPw, setCurrentPw] = createSignal('');
  const [newPw, setNewPw] = createSignal('');
  const [confirmPw, setConfirmPw] = createSignal('');
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [success, setSuccess] = createSignal<string | null>(null);

  async function handleSubmit(e: Event) {
    e.preventDefault();
    setError(null);
    setSuccess(null);

    if (newPw() !== confirmPw()) {
      setError('New passwords do not match.');
      return;
    }

    if (newPw().length < 4) {
      setError('New password must be at least 4 characters.');
      return;
    }

    const username = auth.username();
    if (!username) {
      setError('Not authenticated.');
      return;
    }

    setLoading(true);
    try {
      await changePassword(username, currentPw(), newPw());
      setSuccess('Password updated successfully.');
      setCurrentPw('');
      setNewPw('');
      setConfirmPw('');
    } catch (err: any) {
      setError(err.message || 'Failed to update password.');
    }
    setLoading(false);
  }

  return (
    <section class="password-change-page">
      {/* Asymmetric Header -- matches Stitch v2-password.html */}
      <div class="password-change-header">
        <div>
          <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', color: 'var(--clr-primary)', "letter-spacing": '0.3em', "text-transform": 'uppercase', "margin-bottom": '0.5rem' }}>
            Security / Authentication
          </p>
          <h2 style={{ "font-size": '1.875rem', "font-family": 'var(--font-headline)', "font-weight": '700', "letter-spacing": '-0.025em', color: 'var(--clr-primary)', "margin-bottom": '0' }}>
            Change Password
          </h2>
        </div>
        <div style={{ "text-align": 'right' }}>
          <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', color: 'var(--clr-outline)', "letter-spacing": '0.1em', "text-transform": 'uppercase' }}>
            Registry Node
          </p>
          <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', color: 'var(--clr-on-surface)', "letter-spacing": '0.1em', "text-transform": 'uppercase' }}>
            AUTH_0x44F
          </p>
        </div>
      </div>

      {/* Form Module -- matches Stitch v2-password.html */}
      <div class="password-change-card">
        {/* Decorative icon */}
        <div style={{ position: 'absolute', top: '0', right: '0', padding: '0.75rem', opacity: '0.15', "pointer-events": 'none' }}>
          <span class="material-symbols-outlined" style={{ "font-size": '3rem' }}>lock_reset</span>
        </div>
        {/* Progress bar at bottom */}
        <div class="password-change-progress-track">
          <div class="password-change-progress-fill" />
        </div>

        <form onSubmit={handleSubmit} style={{ display: 'flex', "flex-direction": 'column', gap: '1.5rem' }}>
          <div class="form-group" style={{ "margin-bottom": '0' }}>
            <label class="form-label" style={{ color: 'var(--clr-outline-variant)' }}>Current Password</label>
            <div class="password-input-wrap">
              <input
                type="password"
                class="form-input password-input"
                placeholder="••••••••"
                value={currentPw()}
                onInput={(e) => setCurrentPw(e.currentTarget.value)}
                required
              />
              <span class="material-symbols-outlined password-input-icon">visibility</span>
            </div>
          </div>

          <div class="form-group" style={{ "margin-bottom": '0' }}>
            <label class="form-label" style={{ color: 'var(--clr-outline-variant)' }}>New Password</label>
            <div class="password-input-wrap">
              <input
                type="password"
                class="form-input password-input"
                placeholder="••••••••"
                value={newPw()}
                onInput={(e) => setNewPw(e.currentTarget.value)}
                required
              />
              <span class="material-symbols-outlined password-input-icon">key</span>
            </div>
          </div>

          <div class="form-group" style={{ "margin-bottom": '0' }}>
            <label class="form-label" style={{ color: 'var(--clr-outline-variant)' }}>Confirm New Password</label>
            <div class="password-input-wrap">
              <input
                type="password"
                class="form-input password-input"
                placeholder="••••••••"
                value={confirmPw()}
                onInput={(e) => setConfirmPw(e.currentTarget.value)}
                required
              />
              <span class="material-symbols-outlined password-input-icon">verified_user</span>
            </div>
          </div>

          {/* Status display -- matches Stitch status placeholder */}
          <div class="password-status-bar">
            <Show when={!error() && !success()}>
              <div class="status-led status-led-sm status-led-animated" />
              <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.2em', color: 'var(--clr-on-surface-variant)' }}>
                Awaiting input...
              </p>
            </Show>
            <Show when={error()}>
              <span class="material-symbols-outlined" style={{ "font-size": '14px', color: 'var(--clr-error)' }}>error</span>
              <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.1em', color: 'var(--clr-error)' }}>
                {error()}
              </p>
            </Show>
            <Show when={success()}>
              <span class="material-symbols-outlined" style={{ "font-size": '14px', color: 'var(--clr-success)' }}>check_circle</span>
              <p style={{ "font-size": '0.625rem', "font-family": 'var(--font-label)', "text-transform": 'uppercase', "letter-spacing": '0.1em', color: 'var(--clr-success)' }}>
                {success()}
              </p>
            </Show>
          </div>

          <button
            type="submit"
            class="btn btn-primary"
            style={{ width: '100%', padding: '1rem', "letter-spacing": '0.2em' }}
            disabled={loading()}
          >
            {loading() ? 'Updating...' : 'Update Password'}
          </button>
        </form>
      </div>

      {/* Bottom Technical Overlay -- matches Stitch v2-password.html */}
      <div class="password-change-footer-grid">
        <div class="password-change-footer-item">
          <p style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', color: 'var(--clr-outline)', "letter-spacing": '0.1em', "text-transform": 'uppercase', "margin-bottom": '0.25rem' }}>Security Status</p>
          <p style={{ "font-size": '0.75rem', "font-family": 'var(--font-headline)', color: 'var(--clr-primary)', "font-weight": '500' }}>SECURE_CHANNEL_ESTABLISHED</p>
        </div>
        <div class="password-change-footer-item">
          <p style={{ "font-size": '0.5625rem', "font-family": 'var(--font-label)', color: 'var(--clr-outline)', "letter-spacing": '0.1em', "text-transform": 'uppercase', "margin-bottom": '0.25rem' }}>System Version</p>
          <p style={{ "font-size": '0.75rem', "font-family": 'var(--font-headline)', color: 'var(--clr-on-surface)', "font-weight": '500' }}>V2.4.0-STABLE</p>
        </div>
      </div>
    </section>
  );
}
