// ---------------------------------------------------------------------------
// HTTP transport — the only place fetch() is called for the REST API.
// ---------------------------------------------------------------------------

import { setToken, token } from './token.ts';

export class ApiError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

/**
 * A 401 on a request that carried a Bearer token means the token is expired
 * or revoked: purge it and send the user to the login page (full navigation —
 * we are outside the Router here). Anonymous 401s fall through to the caller,
 * whose guards/empty-states already explain them. The login flow never hits
 * this path: npmLogin() in api.ts deliberately uses a raw fetch, and when the
 * app is already on /login we only purge, never redirect (no loops).
 */
function handleUnauthorized(hadToken: boolean): void {
  if (!hadToken) return;
  setToken(null);
  if (window.location.pathname !== '/login') {
    window.location.assign('/login');
  }
}

async function request<T>(url: string, options: RequestInit = {}): Promise<T> {
  const headers = new Headers(options.headers || {});
  const t = token();
  if (t) headers.set('Authorization', `Bearer ${t}`);
  if (!headers.has('Content-Type') && options.body) {
    headers.set('Content-Type', 'application/json');
  }

  const resp = await fetch(url, { ...options, headers });

  if (!resp.ok) {
    if (resp.status === 401) handleUnauthorized(t !== null);
    let message = `Request failed (${resp.status})`;
    try {
      const data = await resp.json();
      if (data.error) message = data.error;
    } catch {
      // non-JSON error body — keep the generic message
    }
    throw new ApiError(message, resp.status);
  }

  // Some endpoints return an empty body on success.
  const text = await resp.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export const http = {
  get<T>(url: string): Promise<T> {
    return request<T>(url);
  },
  post<T>(url: string, body?: unknown): Promise<T> {
    return request<T>(url, { method: 'POST', body: JSON.stringify(body ?? {}) });
  },
  put<T>(url: string, body?: unknown): Promise<T> {
    return request<T>(url, { method: 'PUT', body: JSON.stringify(body ?? {}) });
  },
  del<T>(url: string): Promise<T> {
    return request<T>(url, { method: 'DELETE' });
  },
  /** Raw text GET (Prometheus metrics). */
  async text(url: string): Promise<string> {
    const headers = new Headers();
    const t = token();
    if (t) headers.set('Authorization', `Bearer ${t}`);
    const resp = await fetch(url, { headers });
    if (!resp.ok) {
      if (resp.status === 401) handleUnauthorized(t !== null);
      throw new ApiError(`Request failed (${resp.status})`, resp.status);
    }
    return resp.text();
  },
};
