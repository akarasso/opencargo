import { describe, expect, it } from 'vitest';
import { severityChip, toVulnReport, type VulnsResponse } from './vulns.ts';

// One payload as GET /api/v1/vulns/{name}/{version} answers it: a pre-upgrade
// row carrying a raw CVSS vector, a pre-upgrade row with no severity, and a
// row classified by the current scanner.
const payload: VulnsResponse = {
  package: '@acme/widget',
  version: '1.0.0',
  scanned_at: '2026-09-17 10:00:00',
  total_deps: 3,
  vulnerable_deps: 3,
  status: 'critical',
  details: [
    {
      dependency: 'lodash',
      version: '4.17.20',
      vuln_id: 'GHSA-vector',
      summary: 'Prototype pollution',
      severity: 'CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H',
    },
    {
      dependency: 'minimist',
      version: '1.2.0',
      vuln_id: 'GHSA-null',
      summary: 'No severity data',
      severity: null,
    },
    {
      dependency: 'event-stream',
      version: '3.3.6',
      vuln_id: 'MAL-2018-1',
      summary: 'Malicious code',
      severity: 'critical',
      score: null,
    },
  ],
};

describe('toVulnReport', () => {
  it('maps the handler shape onto VulnReport', () => {
    const report = toVulnReport(payload);
    expect(report.package_name).toBe('@acme/widget');
    expect(report.version).toBe('1.0.0');
    expect(report.scanned_at).toBe('2026-09-17 10:00:00');
    expect(report.vulnerabilities.length).toBe(3);

    const [vector, missing, mal] = report.vulnerabilities;
    expect(vector.id).toBe('GHSA-vector');
    expect(vector.title).toBe('Prototype pollution');
    expect(vector.description).toBe('lodash@4.17.20');
    expect(vector.score).toBeNull();

    expect(missing.id).toBe('GHSA-null');
    expect(missing.title).toBe('No severity data');
    expect(missing.severity).toBeNull();

    expect(mal.id).toBe('MAL-2018-1');
    expect(mal.title).toBe('Malicious code');
    expect(mal.severity).toBe('critical');
    expect(mal.fixed_in).toBeNull();
  });

  it('keeps a score when the scanner produced one', () => {
    const report = toVulnReport({
      ...payload,
      details: [{ ...payload.details![0], severity: 'high', score: 8.1 }],
    });
    expect(report.vulnerabilities[0].score).toBe(8.1);
  });

  it('turns details: null into an empty list', () => {
    const report = toVulnReport({
      ...payload,
      status: 'not_scanned',
      scanned_at: null,
      details: null,
    });
    expect(report.vulnerabilities).toEqual([]);
    expect(report.scanned_at).toBeNull();
  });

  it('tolerates a rescan body without scanned_at', () => {
    const rescan: VulnsResponse = {
      package: payload.package,
      version: payload.version,
      total_deps: payload.total_deps,
      vulnerable_deps: payload.vulnerable_deps,
      status: payload.status,
      details: payload.details,
    };
    expect(toVulnReport(rescan).scanned_at).toBeNull();
  });
});

describe('severityChip', () => {
  it('classes each entry of the payload', () => {
    const [vector, missing, mal] = toVulnReport(payload).vulnerabilities;
    expect(severityChip(vector.severity)).toBe('chip-neutral');
    expect(severityChip(missing.severity)).toBe('chip-neutral');
    expect(severityChip(mal.severity)).toBe('chip-danger');
  });

  it('maps every label case-insensitively', () => {
    expect(severityChip('HIGH')).toBe('chip-danger');
    expect(severityChip('medium')).toBe('chip-warn');
    expect(severityChip('Moderate')).toBe('chip-warn');
    expect(severityChip('low')).toBe('chip-info');
    expect(severityChip('unknown')).toBe('chip-neutral');
    expect(severityChip(undefined)).toBe('chip-neutral');
  });
});
