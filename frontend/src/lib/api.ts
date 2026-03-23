import auth from './auth.ts';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DashboardData {
  total_packages: number;
  total_versions: number;
  total_downloads: number;
  total_repos: number;
  recent_versions: RecentVersion[];
}

export interface RecentVersion {
  package_name: string;
  version: string;
  published_at: string;
}

export interface Repository {
  name: string;
}

export interface RepositoriesResponse {
  repositories: Repository[];
}

export interface PackageRow {
  name: string;
  latest_version: string;
  description: string;
  downloads: number;
  published_at: string;
}

export interface PackagesResponse {
  packages: PackageRow[];
  total: number;
  page: number;
  page_size: number;
  has_next: boolean;
}

export interface VersionRow {
  version: string;
  size_display: string;
  published_at: string;
}

export interface DistTagRow {
  tag: string;
  version: string;
}

export interface PackageDetail {
  name: string;
  description: string;
  license: string;
  readme_html: string;
  total_downloads: number;
  versions: VersionRow[];
  dist_tags: DistTagRow[];
}

export interface SearchResult {
  name: string;
  latest_version: string;
  description: string;
}

export interface SearchResponse {
  query: string;
  results: SearchResult[];
}

export interface User {
  username: string;
  email: string | null;
  role: string;
  created_at: string;
  updated_at: string;
}

export interface Token {
  id: string;
  name: string;
  prefix: string;
  expires_at: string | null;
  last_used_at: string | null;
  created_at: string;
}

export interface CreateTokenResponse {
  id: string;
  name: string;
  token: string;
  prefix: string;
  expires_at: string | null;
}

export interface AuditEntry {
  id: number;
  user_id: number | null;
  username: string | null;
  action: string;
  target: string | null;
  repository: string | null;
  ip: string | null;
  user_agent: string | null;
  details_json: string | null;
  created_at: string;
}

export interface AuditResponse {
  entries: AuditEntry[];
  page: number;
  size: number;
}

// ---------------------------------------------------------------------------
// Fetch wrapper
// ---------------------------------------------------------------------------

class ApiError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

async function apiFetch<T>(url: string, options: RequestInit = {}): Promise<T> {
  const headers = new Headers(options.headers || {});
  const t = auth.token();
  if (t) {
    headers.set('Authorization', `Bearer ${t}`);
  }
  if (!headers.has('Content-Type') && options.body) {
    headers.set('Content-Type', 'application/json');
  }
  const resp = await fetch(url, { ...options, headers });
  if (!resp.ok) {
    let message = `Request failed: ${resp.status}`;
    try {
      const data = await resp.json();
      if (data.error) message = data.error;
    } catch {
      // ignore
    }
    throw new ApiError(message, resp.status);
  }
  return resp.json();
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

export async function fetchDashboard(): Promise<DashboardData> {
  return apiFetch('/api/v1/dashboard');
}

export async function fetchRepositories(): Promise<RepositoriesResponse> {
  return apiFetch('/api/v1/repositories');
}

export async function fetchPackages(params: {
  q: string;
  repo: string;
  page: number;
}): Promise<PackagesResponse> {
  const search = new URLSearchParams();
  if (params.q) search.set('q', params.q);
  if (params.repo) search.set('repo', params.repo);
  search.set('page', String(params.page));
  return apiFetch(`/api/v1/packages?${search.toString()}`);
}

export async function fetchPackageDetail(name: string): Promise<PackageDetail> {
  return apiFetch(`/api/v1/packages/${name}`);
}

export async function fetchSearch(q: string): Promise<SearchResponse> {
  if (!q) return { query: '', results: [] };
  return apiFetch(`/api/v1/search?q=${encodeURIComponent(q)}`);
}

// ---------------------------------------------------------------------------
// Admin API
// ---------------------------------------------------------------------------

export async function fetchUsers(): Promise<User[]> {
  return apiFetch('/api/v1/users');
}

export async function fetchUser(username: string): Promise<User> {
  return apiFetch(`/api/v1/users/${encodeURIComponent(username)}`);
}

export async function createUser(data: {
  username: string;
  email?: string;
  password: string;
  role?: string;
}): Promise<User> {
  return apiFetch('/api/v1/users', {
    method: 'POST',
    body: JSON.stringify(data),
  });
}

export async function updateUser(
  username: string,
  data: { email?: string; password?: string; role?: string },
): Promise<User> {
  return apiFetch(`/api/v1/users/${encodeURIComponent(username)}`, {
    method: 'PUT',
    body: JSON.stringify(data),
  });
}

export async function deleteUser(username: string): Promise<void> {
  await apiFetch(`/api/v1/users/${encodeURIComponent(username)}`, {
    method: 'DELETE',
  });
}

export async function fetchTokens(username: string): Promise<Token[]> {
  return apiFetch(`/api/v1/users/${encodeURIComponent(username)}/tokens`);
}

export async function createToken(
  username: string,
  data: { name: string; expires_in_days?: number },
): Promise<CreateTokenResponse> {
  return apiFetch(`/api/v1/users/${encodeURIComponent(username)}/tokens`, {
    method: 'POST',
    body: JSON.stringify(data),
  });
}

export async function deleteToken(username: string, tokenId: string): Promise<void> {
  await apiFetch(
    `/api/v1/users/${encodeURIComponent(username)}/tokens/${encodeURIComponent(tokenId)}`,
    { method: 'DELETE' },
  );
}

export async function fetchAudit(page: number = 1, size: number = 50): Promise<AuditResponse> {
  return apiFetch(`/api/v1/system/audit?page=${page}&size=${size}`);
}

export async function fetchMetrics(): Promise<string> {
  const t = auth.token();
  const headers: Record<string, string> = {};
  if (t) headers['Authorization'] = `Bearer ${t}`;
  const resp = await fetch('/metrics', { headers });
  if (!resp.ok) throw new Error('Failed to fetch metrics');
  return resp.text();
}

export async function fetchHealthReady(): Promise<{ status: string }> {
  return apiFetch('/health/ready');
}

// ---------------------------------------------------------------------------
// Dependencies API
// ---------------------------------------------------------------------------

export interface Dependency {
  name: string;
  version_req: string;
  dep_type: string;
}

export interface Dependent {
  name: string;
  version: string;
  dep_type: string;
}

export async function fetchDependencies(name: string): Promise<Dependency[]> {
  return apiFetch(`/api/v1/deps/${name}/dependencies`);
}

export async function fetchDependents(name: string): Promise<Dependent[]> {
  return apiFetch(`/api/v1/deps/${name}/dependents`);
}

// ---------------------------------------------------------------------------
// Vulnerability Scanning API
// ---------------------------------------------------------------------------

export interface VulnEntry {
  id: string;
  severity: string;
  title: string;
  description: string;
  fixed_in: string | null;
}

export interface VulnReport {
  package_name: string;
  version: string;
  vulnerabilities: VulnEntry[];
  scanned_at: string | null;
}

export async function fetchVulns(name: string, version: string): Promise<VulnReport> {
  // For scoped packages (@scope/name), pass as @scope/name (not encoded)
  // The percent-decoding middleware handles %2F → /
  return apiFetch(`/api/v1/vulns/${name}/${encodeURIComponent(version)}`);
}

export async function rescanVulns(name: string, version: string): Promise<VulnReport> {
  return apiFetch(`/api/v1/vulns/${name}/${encodeURIComponent(version)}/rescan`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

// ---------------------------------------------------------------------------
// Promotion API
// ---------------------------------------------------------------------------

export interface PromoteResult {
  message: string;
}

export async function promotePackage(
  name: string,
  version: string,
  from: string,
  to: string,
): Promise<PromoteResult> {
  return apiFetch(`/api/v1/promote/${name}/${encodeURIComponent(version)}`, {
    method: 'POST',
    body: JSON.stringify({ from, to }),
  });
}

// ---------------------------------------------------------------------------
// Password Change API
// ---------------------------------------------------------------------------

export async function changePassword(
  username: string,
  currentPassword: string,
  newPassword: string,
): Promise<{ message: string }> {
  return apiFetch(`/api/v1/users/${encodeURIComponent(username)}/password`, {
    method: 'PUT',
    body: JSON.stringify({ current_password: currentPassword, new_password: newPassword }),
  });
}
