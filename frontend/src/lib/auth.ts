import { createSignal, createRoot } from 'solid-js';

export interface AuthState {
  token: string | null;
  username: string | null;
}

function createAuthStore() {
  const stored = localStorage.getItem('opencargo_token');
  const storedUser = localStorage.getItem('opencargo_username');

  const [token, setToken] = createSignal<string | null>(stored);
  const [username, setUsername] = createSignal<string | null>(storedUser);
  const [checking, setChecking] = createSignal(true);

  async function checkAuth() {
    const t = token();
    if (!t) {
      setChecking(false);
      return;
    }
    try {
      const resp = await fetch('/-/whoami', {
        headers: { Authorization: `Bearer ${t}` },
      });
      if (resp.ok) {
        const data = await resp.json();
        if (data.username && data.username !== 'anonymous') {
          setUsername(data.username);
          localStorage.setItem('opencargo_username', data.username);
        } else {
          logout();
        }
      } else {
        logout();
      }
    } catch {
      logout();
    }
    setChecking(false);
  }

  async function login(user: string, password: string): Promise<string | null> {
    try {
      const resp = await fetch(`/-/user/org.couchdb.user:${encodeURIComponent(user)}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: user, password }),
      });
      const data = await resp.json();
      if (resp.ok && data.ok && data.token) {
        setToken(data.token);
        setUsername(user);
        localStorage.setItem('opencargo_token', data.token);
        localStorage.setItem('opencargo_username', user);
        if (data.must_change_password) {
          localStorage.setItem('opencargo_must_change_password', '1');
        }
        return null;
      }
      return data.error || 'Login failed';
    } catch (e) {
      return 'Network error';
    }
  }

  function mustChangePassword(): boolean {
    return localStorage.getItem('opencargo_must_change_password') === '1';
  }

  function clearMustChangePassword() {
    localStorage.removeItem('opencargo_must_change_password');
  }

  function logout() {
    setToken(null);
    setUsername(null);
    localStorage.removeItem('opencargo_token');
    localStorage.removeItem('opencargo_username');
  }

  function isAuthenticated(): boolean {
    return token() !== null;
  }

  // Check auth on startup
  checkAuth();

  return { token, username, checking, login, logout, isAuthenticated, mustChangePassword, clearMustChangePassword };
}

const auth = createRoot(createAuthStore);
export default auth;
