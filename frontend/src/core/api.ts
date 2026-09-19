// ---------------------------------------------------------------------------
// Typed API client. Pure functions over the HTTP transport — no UI concerns.
// ---------------------------------------------------------------------------

import { http } from './http.ts';
import type {
  AuditResponse,
  CreateTokenResponse,
  DashboardData,
  McpEvidence,
  McpRule,
  McpServers,
  Dependency,
  Dependent,
  MyPermissions,
  PackageDetail,
  PackagesResponse,
  PermissionFlags,
  PermissionGrant,
  PolicyQuery,
  PolicyReport,
  PolicyRules,
  Explanation,
  RawFilesResponse,
  RepositoriesResponse,
  RepositoryDetail,
  RoutingRule,
  RoutingRules,
  SearchResponse,
  StorageStatus,
  InstanceStatus,
  Token,
  User,
  VulnReport,
  Webhook,
  WhoAmI,
} from './types.ts';
import { toVulnReport, type VulnsResponse } from './vulns.ts';

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

export function fetchPolicyReport(q: PolicyQuery): Promise<PolicyReport> {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(q)) {
    if (value !== undefined && value !== '') params.set(key, String(value));
  }
  const qs = params.toString();
  return http.get(`/api/v1/policy/report${qs ? `?${qs}` : ''}`);
}

export function fetchPolicyRules(): Promise<PolicyRules> {
  return http.get('/api/v1/policy/rules');
}

export function fetchMetrics(): Promise<string> {
  return http.text('/metrics');
}

export function fetchHealthReady(): Promise<{ status: string }> {
  return http.get('/health/ready');
}

export function fetchStorageStatus(): Promise<StorageStatus> {
  return http.get('/api/v1/system/storage');
}

export function fetchInstanceStatus(): Promise<InstanceStatus> {
  return http.get('/api/v1/system/instance');
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

export async function fetchVulns(name: string, version: string): Promise<VulnReport> {
  // Scoped packages (@scope/name) go through as-is: the percent-decoding
  // middleware turns %2F back into / before routing.
  return toVulnReport(await http.get<VulnsResponse>(`/api/v1/vulns/${name}/${enc(version)}`));
}

export async function rescanVulns(name: string, version: string): Promise<VulnReport> {
  return toVulnReport(
    await http.post<VulnsResponse>(`/api/v1/vulns/${name}/${enc(version)}/rescan`, {}),
  );
}

export function promotePackage(
  name: string,
  version: string,
  from: string,
  to: string,
): Promise<{ ok: boolean }> {
  return http.post(`/api/v1/promote/${name}/${enc(version)}`, { from, to });
}

// --- MCP governance --------------------------------------------------------------

export function fetchMcpServers(repo: string, state: string, q: string): Promise<McpServers> {
  const search = new URLSearchParams({ state });
  if (q) search.set('q', q);
  return http.get(`/api/v1/mcp/${enc(repo)}/servers?${search}`);
}

export function fetchMcpEvidence(
  repo: string,
  name: string,
  version: string,
): Promise<McpEvidence> {
  const search = new URLSearchParams({ name, version });
  return http.get(`/api/v1/mcp/${enc(repo)}/evidence?${search}`);
}

export function decideMcp(
  repo: string,
  body: {
    name: string;
    version: string;
    state: 'approved' | 'blocked';
    skill?: boolean;
    note?: string;
  },
): Promise<{ decided: number }> {
  return http.post(`/api/v1/mcp/${enc(repo)}/approvals`, body);
}

export function fetchMcpRules(repo: string): Promise<McpRule[]> {
  return http.get(`/api/v1/mcp/${enc(repo)}/allow-rules`);
}

export function addMcpRule(
  repo: string,
  pattern: string,
  effect: 'allow' | 'deny',
): Promise<{ id: number }> {
  return http.post(`/api/v1/mcp/${enc(repo)}/allow-rules`, { pattern, effect });
}

export function deleteMcpRule(repo: string, id: number): Promise<void> {
  return http.del(`/api/v1/mcp/${enc(repo)}/allow-rules/${id}`);
}

export function suppressMcpFinding(
  repo: string,
  pattern: string,
  tool: string,
): Promise<{ id: number }> {
  return http.post(`/api/v1/mcp/${enc(repo)}/suppressions`, { pattern, tool });
}

export function syncMcp(repo: string, full = false): Promise<unknown> {
  return http.post(`/api/v1/mcp/${enc(repo)}/sync`, { full });
}

export function probeMcp(repo: string, name?: string, version?: string): Promise<unknown> {
  return http.post(`/api/v1/mcp/${enc(repo)}/probe`, { name, version });
}

// --- Routing rules -------------------------------------------------------------

export function fetchRoutingRules(): Promise<RoutingRules> {
  return http.get('/api/v1/routing-rules');
}

export function createRoutingRule(rule: {
  name: string;
  format: string;
  patterns: string[];
  except: string[];
  effect: string;
  targets: string[];
}): Promise<RoutingRule> {
  return http.post('/api/v1/routing-rules', rule);
}

export function deleteRoutingRule(name: string): Promise<{ deleted: string; still_refused_by: string[] }> {
  return http.del(`/api/v1/routing-rules/${enc(name)}`);
}

/** The dry run: the same verdict the resolver reaches, on the same snapshot. */
export function explainRoute(repository: string, name: string): Promise<Explanation> {
  return http.post('/api/v1/routing-rules/explain', { repository, name });
// --- Raw files -----------------------------------------------------------------

export function fetchRawFiles(params: {
  repo: string;
  prefix: string;
  page: number;
}): Promise<RawFilesResponse> {
  const search = new URLSearchParams();
  if (params.prefix) search.set('prefix', params.prefix);
  search.set('page', String(params.page));
  return http.get(`/api/v1/raw/${params.repo}/files?${search.toString()}`);
}
