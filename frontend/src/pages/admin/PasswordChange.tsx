import { Show, createSignal } from 'solid-js';
import { useNavigate } from '@solidjs/router';
import Icon from '../../components/Icon.tsx';
import { RequireAuth } from '../../components/guards.tsx';
import { changePassword } from '../../core/api.ts';
import { session } from '../../core/stores/session.ts';
import { toasts } from '../../core/stores/toasts.ts';

export default function PasswordChange() {
  return (
    <RequireAuth>
      <PasswordChangeInner />
    </RequireAuth>
  );
}

function PasswordChangeInner() {
  const navigate = useNavigate();
  const [currentPw, setCurrentPw] = createSignal('');
  const [newPw, setNewPw] = createSignal('');
  const [confirmPw, setConfirmPw] = createSignal('');
  const [show, setShow] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const forced = () => session.user()?.mustChangePassword ?? false;

  async function handleSubmit(e: Event) {
    e.preventDefault();
    setError(null);

    if (newPw() !== confirmPw()) {
      setError("The two new passwords don't match.");
      return;
    }
    if (newPw().length < 8) {
      setError('Use at least 8 characters.');
      return;
    }
    const username = session.user()?.username;
    if (!username) return;

    setLoading(true);
    try {
      // Capture before clearing the flag — reading it afterwards would
      // always be false and skip the redirect.
      const wasForced = forced();
      await changePassword(username, currentPw(), newPw());
      session.clearMustChangePassword();
      toasts.success('Password updated');
      setCurrentPw('');
      setNewPw('');
      setConfirmPw('');
      if (wasForced) navigate('/', { replace: true });
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : 'Could not update the password.');
    }
    setLoading(false);
  }

  return (
    <div class="page-enter" style={{ 'max-width': '460px' }}>
      <div class="page-head">
        <div>
          <h1 class="page-title">Password</h1>
          <p class="page-sub">
            Changes apply to web sign-in and Basic Auth; existing API tokens keep working.
          </p>
        </div>
      </div>

      <Show when={forced()}>
        <div class="alert alert-warn">
          <Icon name="alert-triangle" size={15} />
          <span>
            Your password was generated at first launch. Set your own to unlock the rest of the
            registry — everything else stays read-blocked until you do.
          </span>
        </div>
      </Show>

      <form class="card card-pad" onSubmit={handleSubmit}>
        <Show when={error()}>
          <div class="alert alert-error" role="alert">
            <Icon name="alert-circle" size={15} />
            <span>{error()}</span>
          </div>
        </Show>

        <div class="field">
          <label class="field-label" for="pw-current">
            Current password
          </label>
          <input
            id="pw-current"
            class="input"
            type={show() ? 'text' : 'password'}
            value={currentPw()}
            onInput={(e) => setCurrentPw(e.currentTarget.value)}
            autocomplete="current-password"
            required
          />
        </div>

        <div class="field">
          <label class="field-label" for="pw-new">
            New password
          </label>
          <input
            id="pw-new"
            class="input"
            type={show() ? 'text' : 'password'}
            value={newPw()}
            onInput={(e) => setNewPw(e.currentTarget.value)}
            autocomplete="new-password"
            required
          />
          <div class="field-hint">At least 8 characters. A password manager's suggestion is ideal.</div>
        </div>

        <div class="field">
          <label class="field-label" for="pw-confirm">
            Confirm new password
          </label>
          <input
            id="pw-confirm"
            class="input"
            type={show() ? 'text' : 'password'}
            value={confirmPw()}
            onInput={(e) => setConfirmPw(e.currentTarget.value)}
            autocomplete="new-password"
            required
          />
        </div>

        <label class="checkbox-row" style={{ 'margin-bottom': '14px' }}>
          <input type="checkbox" checked={show()} onChange={() => setShow((v) => !v)} />
          Show passwords
        </label>

        <button class="btn btn-primary" style={{ width: '100%' }} disabled={loading()}>
          {loading() ? 'Updating…' : 'Update password'}
        </button>
      </form>
    </div>
  );
}
