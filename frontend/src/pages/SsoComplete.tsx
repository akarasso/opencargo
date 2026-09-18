import { Show, createSignal, onMount } from 'solid-js';
import { A, useNavigate } from '@solidjs/router';
import { codeFromHash, ssoErrorMessage, ssoExchange } from '../core/sso.ts';
import { session } from '../core/stores/session.ts';

export default function SsoComplete() {
  const navigate = useNavigate();
  const [error, setError] = createSignal<string | null>(null);

  onMount(async () => {
    const code = codeFromHash();
    history.replaceState(null, '', window.location.pathname);
    if (!code) {
      setError(ssoErrorMessage('state_mismatch'));
      return;
    }
    try {
      const s = await ssoExchange(code);
      await session.loginWithToken(s.token);
      navigate(s.return_to || '/', { replace: true });
    } catch (e) {
      setError(ssoErrorMessage(e instanceof Error ? e.message : String(e)));
    }
  });

  return (
    <div class="login-page">
      <div class="login-box">
        <div class="login-card">
          <Show when={error()} fallback={<div class="dim">Signing you in…</div>}>
            <div class="alert alert-error" role="alert">
              {error()}
            </div>
            <A class="btn btn-ghost" href="/login">
              Back to sign in
            </A>
          </Show>
        </div>
      </div>
    </div>
  );
}
