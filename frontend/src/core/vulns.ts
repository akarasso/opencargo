import type { VulnEntry, VulnReport } from './types.ts';

/** One finding as the backend stores it: a dependency and one advisory. */
export interface VulnDetail {
  dependency: string;
  version: string;
  vuln_id: string;
  summary: string;
  /** A lowercase label; pre-upgrade rows carry a raw CVSS vector or null. */
  severity: string | null;
  score?: number | null;
}

/** The body of GET /api/v1/vulns/{name}/{version} and of its rescan POST. */
export interface VulnsResponse {
  package: string;
  version: string;
  scanned_at?: string | null;
  total_deps: number;
  vulnerable_deps: number;
  status: string;
  details: VulnDetail[] | null;
}

export function toVulnReport(raw: VulnsResponse): VulnReport {
  return {
    package_name: raw.package,
    version: raw.version,
    scanned_at: raw.scanned_at ?? null,
    vulnerabilities: (raw.details ?? []).map(toEntry),
  };
}

function toEntry(d: VulnDetail): VulnEntry {
  return {
    id: d.vuln_id,
    severity: d.severity,
    score: d.score ?? null,
    title: d.summary,
    description: `${d.dependency}@${d.version}`,
    fixed_in: null,
  };
}

const SEVERITY_CHIP: Record<string, string> = {
  critical: 'chip-danger',
  high: 'chip-danger',
  medium: 'chip-warn',
  moderate: 'chip-warn',
  low: 'chip-info',
  unknown: 'chip-neutral',
};

/** Chip class for a severity label; anything else (a CVSS vector, null) is neutral. */
export function severityChip(severity: string | null | undefined): string {
  return SEVERITY_CHIP[(severity ?? '').toLowerCase()] ?? 'chip-neutral';
}
