import { Show, createResource, createSignal } from 'solid-js';
import { A, useNavigate } from '@solidjs/router';
import { RequireAuth } from '../components/guards.tsx';
import { codeFromHash, linkConfirm, linkPending } from '../core/sso.ts';
import { toasts } from '../core/stores/toasts.ts';

/**
 * The confirmation screen of a link: it shows who the provider says this is
 * and which account it would be tied to, and sends nothing until a human
 * presses the button.
 */
export default function SsoLink() {
  const navigate = useNavigate();
  const code = codeFromHash();
  history.replaceState(null, '', window.location.pathname);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [proposal] = createResource(
    () => code,
    (c) => linkPending(c),
  );

  async function confirm() {
    if (!code || busy()) return;
    setBusy(true);
    try {
      await linkConfirm(code);
      toasts.success('Identity linked');
      navigate('/account/sso', { replace: true });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <RequireAuth>
      <div class="page-enter">
        <div class="page-head">
          <h1 class="page-title">Link an identity</h1>
        </div>
        <Show when={error() || proposal.error}>
          <div class="alert alert-error" role="alert">
            {error() ?? 'This link request is no longer valid.'}
          </div>
        </Show>
        <Show when={proposal()}>
          {(p) => (
            <div class="table-card" style={{ padding: '16px', 'max-width': '560px' }}>
              <dl class="kv">
                <dt>Provider</dt>
                <dd class="mono">{p().provider}</dd>
                <dt>Issuer (iss)</dt>
                <dd class="mono">{p().issuer}</dd>
                <dt>Subject (sub)</dt>
                <dd class="mono">{p().subject}</dd>
                <dt>E-mail</dt>
                <dd>
                  <span class="mono">{p().email ?? '—'}</span>{' '}
                  <span class="dim small">
                    {p().email_verified ? '(verified)' : '(not verified)'}
                  </span>
                </dd>
                <dt>Account</dt>
                <dd class="mono">{p().target}</dd>
              </dl>
              <Show when={p().proposal}>
                <p class="dim small">The provider is authoritative for this address's domain.</p>
              </Show>
              <div class="row" style={{ 'margin-top': '12px' }}>
                <button class="btn btn-primary" disabled={busy()} onClick={() => void confirm()}>
                  Link to {p().target}
                </button>
                <A class="btn btn-ghost" href="/account/sso">
                  Cancel
                </A>
              </div>
            </div>
          )}
        </Show>
      </div>
    </RequireAuth>
  );
}
