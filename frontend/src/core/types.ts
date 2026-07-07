// ---------------------------------------------------------------------------
// Shared API types — the single source of truth for backend response shapes.
// Core layer only: no solid-js, no DOM, no JSX imports here.
// ---------------------------------------------------------------------------

export type Role = 'admin' | 'publisher' | 'reader' | 'anonymous';

export interface WhoAmI {
  username: string;
  role: Role;
  must_change_password: boolean;
}

export interface SessionUser {
  username: string;
  role: Role;
  mustChangePassword: boolean;
}

// --- Registry -------------------------------------------------------------

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

export type RepoType = 'hosted' | 'proxy' | 'group';
export type RepoFormat = 'npm' | 'cargo' | 'oci' | 'go';
export type RepoVisibility = 'public' | 'private';

export interface Repository {
  name: string;
  type: RepoType;
  format: RepoFormat;
  visibility: RepoVisibility;
  upstream: string | null;
}

export interface RepositoryDetail extends Repository {
  id: number;
  config: string | null;
  created_at: string;
  updated_at: string;
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

// --- Users / auth ----------------------------------------------------------

export interface User {
  username: string;
  email: string | null;
  role: Role;
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

// --- Permissions ------------------------------------------------------------

/** One effective-permission row from GET /api/v1/me/permissions. */
export interface EffectivePermission {
  repository: string;
  type: RepoType;
  format: RepoFormat;
  visibility: RepoVisibility;
  can_read: boolean;
  can_write: boolean;
  can_delete: boolean;
  can_admin: boolean;
  /** Which rule produced these rights. */
  source: 'admin' | 'grant' | 'role' | 'anonymous';
}

export interface MyPermissions {
  username: string;
  role: Role;
  permissions: EffectivePermission[];
}

/** One explicit grant from GET /api/v1/users/{u}/permissions (admin view). */
export interface PermissionGrant {
  repository: string;
  repository_id: number;
  can_read: boolean;
  can_write: boolean;
  can_delete: boolean;
  can_admin: boolean;
}

export interface PermissionFlags {
  can_read: boolean;
  can_write: boolean;
  can_delete: boolean;
  can_admin: boolean;
}

// --- Audit -------------------------------------------------------------------

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

// --- Webhooks ----------------------------------------------------------------

export interface Webhook {
  id: number;
  url: string;
  events: string[];
  active: boolean;
  created_at: string;
  updated_at: string;
}

// --- Dependencies / vulnerabilities -------------------------------------------

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

// --- Real-time events ----------------------------------------------------------

/** Frame received on the events WebSocket. */
export interface WsEvent {
  type: string;
  data?: Record<string, unknown>;
  ts?: string;
  /** hello frame */
  username?: string;
  role?: string;
  anonymous?: boolean;
}
