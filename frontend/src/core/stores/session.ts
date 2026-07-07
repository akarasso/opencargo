// ---------------------------------------------------------------------------
// Session store — who am I, what can I do.
//
// Reactive singleton (createRoot): holds the authenticated user (with role),
// their effective per-repo permissions, and the login/logout flows. The
// WebSocket is (re)connected on every auth change since its identity is
// fixed per connection.
// ---------------------------------------------------------------------------

import { createResource, createRoot, createSignal } from 'solid-js';
import { fetchMyPermissions, npmLogin, whoami } from '../api.ts';
import { setToken, token } from '../token.ts';
import { connectWs, onEvent, reconnectWs } from '../ws.ts';
import type { EffectivePermission, MyPermissions, SessionUser } from '../types.ts';

function createSessionStore() {
  const [user, setUser] = createSignal<SessionUser | null>(null);
  /** True until the startup whoami round-trip settles — gate redirects on it. */
  const [checking, setChecking] = createSignal(true);

  const [permissions, { refetch: refetchPermissions }] = createResource<MyPermissions | null>(
    async () => {
      try {
        return await fetchMyPermissions();
      } catch {
        return null;
      }
    },
  );

  async function refreshIdentity(): Promise<void> {
    if (!token()) {
      setUser(null);
      setChecking(false);
      return;
    }
    try {
      const me = await whoami();
      if (me.username && me.username !== 'anonymous') {
        setUser({
          username: me.username,
          role: me.role,
          mustChangePassword: me.must_change_password,
        });
      } else {
        // Token no longer valid.
        setToken(null);
        setUser(null);
      }
    } catch {
      // Network failure: keep the token, stay optimistic; WS status will
      // surface connectivity problems.
    }
    setChecking(false);
  }

  async function login(username: string, password: string): Promise<string | null> {
    const resp = await npmLogin(username, password).catch(() => null);
    if (!resp || !resp.ok || !resp.token) {
      return resp?.error || 'Login failed';
    }
    setToken(resp.token);
    setUser({
      username,
      role: 'reader', // provisional — whoami below fills in the real role
      mustChangePassword: Boolean(resp.must_change_password),
    });
    await refreshIdentity();
    // Keep must_change_password from the login response: whoami may run
    // before the flag propagates, and the login response is authoritative.
    if (resp.must_change_password) {
      const u = user();
      if (u) setUser({ ...u, mustChangePassword: true });
    }
    reconnectWs();
    void refetchPermissions();
    return null;
  }

  function logout(): void {
    setToken(null);
    setUser(null);
    reconnectWs();
    void refetchPermissions();
  }

  /** Called after a successful forced password change. */
  function clearMustChangePassword(): void {
    const u = user();
    if (u) setUser({ ...u, mustChangePassword: false });
  }

  const isAuthenticated = () => user() !== null;
  const isAdmin = () => user()?.role === 'admin';

  /** Effective rights on one repository (from /me/permissions). */
  function permissionFor(repo: string): EffectivePermission | undefined {
    return permissions()?.permissions.find((p) => p.repository === repo);
  }

  /** True when the user can publish somewhere at all. */
  const canWriteAnywhere = () =>
    permissions()?.permissions.some((p) => p.can_write) ?? false;

  // --- Live updates ---------------------------------------------------------
  // Rights change server-side → refresh what this session can see/do.
  onEvent('permissions.changed', (ev) => {
    const who = ev.data?.username as string | undefined;
    if (!who || who === user()?.username || isAdmin()) {
      void refetchPermissions();
      void refreshIdentity(); // role may have changed too
    }
  });
  onEvent('repositories.changed', () => void refetchPermissions());
  onEvent('$connected', () => void refetchPermissions());
  onEvent('$resync', () => void refetchPermissions());

  // Boot: resolve identity, then open the socket (anonymous or not).
  void refreshIdentity().then(connectWs);

  return {
    user,
    checking,
    permissions,
    refetchPermissions,
    permissionFor,
    canWriteAnywhere,
    login,
    logout,
    clearMustChangePassword,
    isAuthenticated,
    isAdmin,
  };
}

export const session = createRoot(createSessionStore);
