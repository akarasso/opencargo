import { For, Show, createResource, createSignal } from 'solid-js';
import { RequireAuth } from '../components/guards.tsx';
import { identitiesOf, linkStart, ssoProviders, unlink, type LinkedIdentity } from '../core/sso.ts';
import { session } from '../core/stores/session.ts';
import { toasts } from '../core/stores/toasts.ts';

export default function SsoAccount() {
  const me = () => session.user()?.username ?? '';
  const [providers] = createResource(ssoProviders);
  const [linked, { refetch }] = createResource(me, (u) => identitiesOf(u));
  const [provider, setProvider] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [error, setError] = createSignal<string | null>(null);

  async function begin(e: Event) {
    e.preventDefault();
    setError(null);
    const name = provider() || providers()?.providers[0];
    if (!name) return;
    try {
      const { location } = await linkStart(name, password());
      window.location.assign(location);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function remove(id: LinkedIdentity) {
    await unlink(me(), id);
    toasts.success('Identity unlinked');
    void refetch();
  }

  return (
    <RequireAuth>
      <div class="page-enter">
        <div class="page-head">
          <h1 class="page-title">Single sign-on</h1>
        </div>
        <div class="table-card">
          <table class="table">
            <thead>
              <tr>
                <th>Provider</th>
                <th>Subject</th>
                <th>E-mail</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={linked()?.identities ?? []}>
                {(id) => (
                  <tr>
                    <td class="mono">{id.provider}</td>
                    <td class="mono">{id.subject}</td>
                    <td>{id.email ?? '—'}</td>
                    <td>
                      <Show when={!id.provisioned}>
                        <button class="btn btn-ghost" onClick={() => void remove(id)}>
                          Unlink
                        </button>
                      </Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
        <Show when={(providers()?.providers.length ?? 0) > 0 && !session.isAdmin()}>
          <form
            class="login-card"
            style={{ 'max-width': '420px', 'margin-top': '16px' }}
            onSubmit={begin}
          >
            <Show when={error()}>
              <div class="alert alert-error" role="alert">
                {error()}
              </div>
            </Show>
            <div class="field">
              <label class="field-label" for="sso-provider">
                Provider
              </label>
              <select
                id="sso-provider"
                class="input"
                onChange={(e) => setProvider(e.currentTarget.value)}
              >
                <For each={providers()?.providers ?? []}>
                  {(p) => <option value={p}>{p}</option>}
                </For>
              </select>
            </div>
            <div class="field">
              <label class="field-label" for="sso-password">
                Current password
              </label>
              <input
                id="sso-password"
                class="input"
                type="password"
                autocomplete="current-password"
                value={password()}
                onInput={(e) => setPassword(e.currentTarget.value)}
                required
              />
            </div>
            <button class="btn btn-primary">Link an identity</button>
          </form>
        </Show>
      </div>
    </RequireAuth>
  );
}
