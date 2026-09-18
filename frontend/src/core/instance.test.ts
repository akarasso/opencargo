import { describe, expect, it } from 'vitest';
import { backupTone, leaseTone } from './instance.ts';
import type { InstanceStatus } from './types.ts';

// One body as GET /api/v1/system/instance answers it.
const body: InstanceStatus = JSON.parse(`{
  "owner": "3f9a2c1e",
  "version": "0.1.0",
  "acquired_at": "2026-09-18T01:00:00+00:00",
  "renewed_at": "2026-09-18T03:59:50+00:00",
  "lease": "held",
  "last_backup_at": "2026-09-18T03:00:00+00:00",
  "last_sweep_at": null,
  "last_backup_wal": "truncated",
  "incomplete_snapshots": 0,
  "shutdown_grace_secs": 30,
  "endpoint_drain_secs": 0,
  "open_http_connections": 2
}`);

const now = new Date('2026-09-18T04:00:00Z');

describe('instance status', () => {
  it('parses the body the server sends', () => {
    expect(body.last_backup_wal).toBe('truncated');
    expect(body.incomplete_snapshots).toBe(0);
    expect(backupTone(body, now)).toBe('ok');
    expect(leaseTone(body)).toBe('ok');
  });

  it('goes amber on a lost lease, and grey when disabled', () => {
    expect(leaseTone({ ...body, lease: 'lost' })).toBe('amber');
    expect(leaseTone({ ...body, lease: 'disabled' })).toBe('grey');
  });

  it('goes amber on no backup, an old one, a busy WAL or an interrupted run', () => {
    expect(backupTone({ ...body, last_backup_at: null }, now)).toBe('amber');
    expect(backupTone({ ...body, last_backup_at: '2026-09-01T03:00:00+00:00' }, now)).toBe('amber');
    expect(backupTone({ ...body, last_backup_wal: 'busy' }, now)).toBe('amber');
    expect(backupTone({ ...body, incomplete_snapshots: 1 }, now)).toBe('amber');
  });
});
