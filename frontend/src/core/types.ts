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
export type RepoFormat = 'npm' | 'cargo' | 'oci' | 'go' | 'pypi' | 'maven' | 'nuget' | 'mcp';
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
  /** 'hosted': a package this server holds; 'cached': one a proxy member served. */
  source: 'hosted' | 'cached';
  repository: string;
  last_seen: string | null;
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

// --- Policy report -------------------------------------------------------------

export type PolicyVerdictKind = 'pass' | 'would_block' | 'unknown' | 'not_applicable';

export interface PolicyVerdict {
  rule: string;
  verdict: PolicyVerdictKind;
  reason: string;
}

export interface PolicyEntry {
  id: number;
  created_at: string;
  requested_repo: string;
  member_repo: string;
  format: string;
  name: string;
  version: string | null;
  digest: string | null;
  actor: string;
  actor_kind: 'token' | 'user' | 'static' | 'anonymous';
  user_id: number | null;
  published_at: string | null;
  would_block: boolean;
  unknown: boolean;
  verdicts: PolicyVerdict[];
}

export interface PolicyRuleTotals {
  would_block: number;
  unknown: number;
  pass: number;
  not_applicable: number;
}

export interface PolicyTotals {
  resolutions: number;
  would_block: number;
  unknown: number;
  by_rule: Record<string, PolicyRuleTotals>;
}

export interface PolicyReport {
  since: string;
  page: number;
  size: number;
  /** Admin view only: the process-lifetime queue-drop counter, outside `totals` on purpose. */
  process?: { dropped_since_start: number };
  totals: PolicyTotals;
  entries: PolicyEntry[];
}

export interface PolicyQuery {
  since?: string;
  repo?: string;
  rule?: string;
  page?: number;
  size?: number;
}

export interface PolicyRuleConfig {
  min_release_age: string | null;
  osv_severity: string | null;
  install_scripts: boolean;
  typosquat: boolean;
  fetch_missing_facts: boolean;
}

export interface PolicyRules {
  osv_enabled: boolean;
  recording: string[];
  repositories: Record<string, PolicyRuleConfig>;
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
  /** A lowercase label, or null when the scan predates severity classification. */
  severity: string | null;
  score: number | null;
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

// --- MCP governance --------------------------------------------------------------

export type McpState = 'approved' | 'pending' | 'drifted' | 'blocked';

export interface McpServerRow {
  name: string;
  version: string;
  member: string;
  status: string;
  isLatest: boolean;
  hosted: boolean;
  state: McpState;
  drift: string;
  driftedRemote: string | null;
  endpoints: { approved: number; total: number };
  transports: { packages: string; remotes: string };
  toolsSource: 'declared' | 'probe' | 'attested' | null;
  findings: { high: number; medium: number };
  syncedAt: string;
}

export interface McpServers {
  repository: string;
  servers: McpServerRow[];
  sync: {
    lastRunAt: string | null;
    lastError: string | null;
    skipped: number;
    consecutiveFailures: number;
  } | null;
}

export interface McpFinding {
  id: number;
  pattern: string;
  confidence: 'high' | 'medium';
  promotedBy: string | null;
  field: string;
  tool: string;
  span: [number, number];
  excerpt: string;
  suppressed: boolean;
}

export interface McpSurface {
  id: number;
  source: string;
  remoteUrl: string;
  toolsSha256: string | null;
  permissionsSha256: string;
  capturedAt: string;
  tools: { name: string; description?: string | null }[];
  headerParams: { tool: string; pointer: string; header?: string; why?: string; valid: boolean }[];
}

export interface McpEvidence {
  row: McpServerRow;
  server: Record<string, unknown>;
  permissions: Record<string, unknown>;
  surfaces: McpSurface[];
  findings: McpFinding[];
  approvals: {
    remoteUrl: string;
    decision: string;
    decidedBy: string;
    decidedAt: string;
    note: string | null;
    own: boolean;
  }[];
  probeRuns: {
    remoteUrl: string;
    ranAt: string;
    ok: boolean;
    protocolVersion: string | null;
    error: string | null;
  }[];
}

export interface McpRule {
  id: number;
  pattern: string;
  effect: 'allow' | 'deny';
}

/** `GET /api/v1/system/storage`: the adapter and its health, never where it points. */
export interface StorageStatus {
  backend: 'fs' | 's3';
  identity: string;
  ready: boolean;
  multipart_in_flight: number;
  reclaim_candidates: number;
  reclaim_prefixes: number;
}

/** `GET /api/v1/system/instance`: the one instance, never a host or a path.
 * `open_http_connections` excludes WebSocket clients. */
export interface InstanceStatus {
  owner: string;
  version: string;
  acquired_at: string | null;
  renewed_at: string | null;
  lease: 'held' | 'lost' | 'disabled';
  last_backup_at: string | null;
  last_sweep_at: string | null;
  last_backup_wal: 'truncated' | 'busy' | null;
  incomplete_snapshots: number;
  shutdown_grace_secs: number;
  endpoint_drain_secs: number;
  open_http_connections: number;
}

// --- Routing rules -------------------------------------------------------------

/** One rule as the admin API returns it; targets are repository incarnations. */
export interface RoutingRule {
  name: string;
  format: string;
  patterns: string[];
  except: string[];
  effect: 'deny' | 'allow_hosted' | 'allow_members';
  targets: string[];
  created_at: string;
  updated_at: string;
}

export interface RoutingRules {
  rules: RoutingRule[];
  snapshot_version: number;
}

export interface ExplainedMember {
  name: string;
  kind: string;
  admitted: boolean;
  /** Every rule that refuses, in name order — never one chosen out of them. */
  refused_by: string[];
  /** Refused because this node has not refreshed its rules within the bound. */
  stale: boolean;
}

export interface Explanation {
  match_key: string;
  ident_key: string;
  snapshot_version: number;
  members: ExplainedMember[];
}
