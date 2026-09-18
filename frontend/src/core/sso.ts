// ---------------------------------------------------------------------------
// SSO client. The handoff and link calls send the attempt cookie (same
// origin) and a JSON body, which a cross-site form cannot forge.
// ---------------------------------------------------------------------------

import { http } from './http.ts';

const enc = encodeURIComponent;

export interface SsoProviders {
  providers: string[];
  password_mode: 'enabled' | 'admins_only' | 'disabled';
}

export interface SsoSession {
  token: string;
  username: string;
  expires_at: string;
  return_to: string;
}

export interface LinkProposal {
  provider: string;
  issuer: string;
  subject: string;
  email: string | null;
  email_verified: boolean;
  proposal: boolean;
  target: string;
}

export interface LinkedIdentity {
  provider: string;
  issuer: string;
  subject: string;
  email: string | null;
  provisioned: boolean;
  disabled: boolean;
  linked_at: string;
  last_login_at: string;
}

const SSO_ERRORS: Record<string, string> = {
  state_mismatch: 'The sign-in attempt expired or was not started here. Try again.',
  denied: 'Your identity provider says you may not use this registry.',
  rejected: 'Your identity provider sent an answer that could not be read.',
  disabled: 'This account is disabled.',
  name_collision: 'A local account already uses this name. Link it from your account page.',
  unavailable: 'The identity provider is unavailable. Try again later.',
  idp_error: 'The identity provider refused the sign-in.',
  unknown_provider: 'Unknown identity provider.',
};

export function ssoErrorMessage(code: string): string {
  return SSO_ERRORS[code] ?? `Sign-in failed (${code}).`;
}

export function startUrl(provider: string, returnTo = '/'): string {
  return `/api/v1/auth/sso/${enc(provider)}/start?return_to=${enc(returnTo)}`;
}

export async function ssoProviders(): Promise<SsoProviders> {
  const resp = await fetch('/api/v1/auth/sso/providers');
  if (!resp.ok) return { providers: [], password_mode: 'enabled' };
  return resp.json();
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const resp = await fetch(url, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) throw new Error(data.error ?? `failed (${resp.status})`);
  return data as T;
}

export function ssoExchange(code: string): Promise<SsoSession> {
  return postJson('/api/v1/auth/sso/exchange', { code });
}

export function linkStart(provider: string, password: string): Promise<{ location: string }> {
  return http.post('/api/v1/auth/sso/link/start', { provider, password });
}

export function linkPending(code: string): Promise<LinkProposal> {
  return http.get(`/api/v1/auth/sso/link/pending?code=${enc(code)}`);
}

export function linkConfirm(code: string): Promise<void> {
  return http.post('/api/v1/auth/sso/link/confirm', { code });
}

export function identitiesOf(username: string): Promise<{ identities: LinkedIdentity[] }> {
  return http.get(`/api/v1/users/${enc(username)}/identities`);
}

export function unlink(username: string, id: LinkedIdentity): Promise<void> {
  const q = `provider=${enc(id.provider)}&issuer=${enc(id.issuer)}&subject=${enc(id.subject)}`;
  return http.del(`/api/v1/users/${enc(username)}/identities?${q}`);
}

export function serverLogout(): Promise<{ end_session_url: string | null }> {
  return http.post('/api/v1/auth/logout', {});
}

/** The handoff code the callback put in the URL fragment, which no server sees. */
export function codeFromHash(): string | null {
  const m = /(?:^|[#&])code=([^&]+)/.exec(window.location.hash);
  return m ? decodeURIComponent(m[1]) : null;
}
