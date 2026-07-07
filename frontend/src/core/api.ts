// ---------------------------------------------------------------------------
// Typed API client. Pure functions over the HTTP transport — no UI concerns.
// ---------------------------------------------------------------------------

import { http } from './http.ts';
import type {
  AuditResponse,
  CreateTokenResponse,
  DashboardData,
  Dependency,
  Dependent,
  MyPermissions,
  PackageDetail,
  PackagesResponse,
  PermissionFlags,
  PermissionGrant,
  RepositoriesResponse,
  RepositoryDetail,
  SearchResponse,
  Token,
  User,
  VulnReport,
  Webhook,
  WhoAmI,
} from './types.ts';

const enc = encodeURIComponent;

// --- Session -----------------------------------------------------------------

export function whoami(): Promise<WhoAmI> {
  return http.get('/-/whoami');
}

export function fetchMyPermissions(): Promise<MyPermissions> {
  return http.get('/api/v1/me/permissions');
}

export async function npmLogin(
  username: string,
  password: string,
): Promise<{ ok?: boolean; token?: string; must_change_password?: boolean; error?: string }> {
  // Raw fetch: this endpoint authenticates with the body, not a Bearer token,
  // and errors must be readable rather than thrown.
  const resp = await fetch(`/-/user/org.couchdb.user:${enc(username)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: username, password }),
  });
  try {
    return await resp.json();
  } catch {
    return { error: `Login failed (${resp.status})` };
  }
}

// --- Registry ------------------------------------------------------------------

export function fetchDashboard(): Promise<DashboardData> {
  return http.get('/api/v1/dashboard');
}

export function fetchRepositories(): Promise<RepositoriesResponse> {
  return http.get('/api/v1/repositories');
}

export function fetchPackages(params: {
  q: string;
  repo: string;
  page: number;
}): Promise<PackagesResponse> {
  const search = new URLSearchParams();
  if (params.q) search.set('q', params.q);
  if (params.repo) search.set('repo', params.repo);
  search.set('page', String(params.page));
  return http.get(`/api/v1/packages?${search.toString()}`);
}

export function fetchPackageDetail(name: string): Promise<PackageDetail> {
  return http.get(`/api/v1/packages/${name}`);
}

export function fetchSearch(q: string): Promise<SearchResponse> {
  if (!q) return Promise.resolve({ query: '', results: [] });
  return http.get(`/api/v1/search?q=${enc(q)}`);
}

// --- Repository administration ---------------------------------------------------

export function fetchRepositoryDetail(name: string): Promise<RepositoryDetail> {
  return http.get(`/api/v1/repositories/${enc(name)}`);
}

export function createRepository(data: {
  name: string;
  type: string;
  format: string;
  visibility: string;
  upstream?: string;
  members?: string[];
}): Promise<RepositoryDetail> {
  return http.post('/api/v1/repositories', data);
}

export function updateRepository(
  name: string,
  data: { visibility?: string; upstream?: string; members?: string[] },
): Promise<RepositoryDetail> {
  return http.put(`/api/v1/repositories/${enc(name)}`, data);
}

export function deleteRepository(name: string): Promise<void> {
  return http.del(`/api/v1/repositories/${enc(name)}`);
}

export function purgeRepositoryCache(name: string): Promise<{ ok: boolean; message: string }> {
  return http.post(`/api/v1/repositories/${enc(name)}/purge-cache`);
}

// --- Users ------------------------------------------------------------------------

export function fetchUsers(): Promise<User[]> {
  return http.get('/api/v1/users');
}

export function fetchUser(username: string): Promise<User> {
  return http.get(`/api/v1/users/${enc(username)}`);
}

export function createUser(data: {
  username: string;
  email?: string;
  password: string;
  role?: string;
}): Promise<User> {
  return http.post('/api/v1/users', data);
}

export function updateUser(
  username: string,
  data: { email?: string; password?: string; role?: string },
): Promise<User> {
  return http.put(`/api/v1/users/${enc(username)}`, data);
}

export function deleteUser(username: string): Promise<void> {
  return http.del(`/api/v1/users/${enc(username)}`);
}

export function changePassword(
  username: string,
  currentPassword: string,
  newPassword: string,
): Promise<{ ok: boolean }> {
  return http.put(`/api/v1/users/${enc(username)}/password`, {
    current_password: currentPassword,
    new_password: newPassword,
  });
}

// --- Tokens -------------------------------------------------------------------------

export function fetchTokens(username: string): Promise<Token[]> {
  return http.get(`/api/v1/users/${enc(username)}/tokens`);
}

export function createToken(
  username: string,
  data: { name: string; expires_in_days?: number },
): Promise<CreateTokenResponse> {
  return http.post(`/api/v1/users/${enc(username)}/tokens`, data);
}

export function deleteToken(username: string, tokenId: string): Promise<void> {
  return http.del(`/api/v1/users/${enc(username)}/tokens/${enc(tokenId)}`);
}

// --- Permissions (admin) ---------------------------------------------------------------

export function fetchUserPermissions(
  username: string,
): Promise<{ permissions: PermissionGrant[] }> {
  return http.get(`/api/v1/users/${enc(username)}/permissions`);
}

export function setUserPermission(
  username: string,
  repo: string,
  flags: PermissionFlags,
): Promise<PermissionFlags & { ok: boolean }> {
  return http.put(`/api/v1/users/${enc(username)}/permissions/${enc(repo)}`, flags);
}

export function deleteUserPermission(username: string, repo: string): Promise<void> {
  return http.del(`/api/v1/users/${enc(username)}/permissions/${enc(repo)}`);
}

// --- Audit / system ------------------------------------------------------------------------

export function fetchAudit(page = 1, size = 50): Promise<AuditResponse> {
  return http.get(`/api/v1/system/audit?page=${page}&size=${size}`);
}

export function fetchMetrics(): Promise<string> {
  return http.text('/metrics');
}

export function fetchHealthReady(): Promise<{ status: string }> {
  return http.get('/health/ready');
}

// --- Webhooks ---------------------------------------------------------------------------------

export function fetchWebhooks(): Promise<{ webhooks: Webhook[] }> {
  return http.get('/api/v1/webhooks');
}

export function createWebhook(data: {
  url: string;
  events: string[];
  secret?: string;
}): Promise<Webhook> {
  return http.post('/api/v1/webhooks', data);
}

export function updateWebhook(
  id: number,
  data: { url?: string; events?: string[]; secret?: string | null; active?: boolean },
): Promise<Webhook> {
  return http.put(`/api/v1/webhooks/${id}`, data);
}

export function deleteWebhook(id: number): Promise<void> {
  return http.del(`/api/v1/webhooks/${id}`);
}

export function testWebhook(id: number): Promise<{ ok: boolean; message: string }> {
  return http.post(`/api/v1/webhooks/${id}/test`);
}

// --- Dependencies / vulnerabilities / promotion --------------------------------------------------

export function fetchDependencies(name: string): Promise<Dependency[]> {
  return http.get(`/api/v1/deps/${name}/dependencies`);
}

export function fetchDependents(name: string): Promise<Dependent[]> {
  return http.get(`/api/v1/deps/${name}/dependents`);
}

export function fetchVulns(name: string, version: string): Promise<VulnReport> {
  // Scoped packages (@scope/name) go through as-is: the percent-decoding
  // middleware turns %2F back into / before routing.
  return http.get(`/api/v1/vulns/${name}/${enc(version)}`);
}

export function rescanVulns(name: string, version: string): Promise<VulnReport> {
  return http.post(`/api/v1/vulns/${name}/${enc(version)}/rescan`, {});
}

export function promotePackage(
  name: string,
  version: string,
  from: string,
  to: string,
): Promise<{ ok: boolean }> {
  return http.post(`/api/v1/promote/${name}/${enc(version)}`, { from, to });
}
