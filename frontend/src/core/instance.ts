import type { InstanceStatus } from './types.ts';
import { parseDate } from './format.ts';

export type Tone = 'ok' | 'amber' | 'grey';

const WEEK_MS = 7 * 86_400_000;

/** A lost lease is visible only here: it is never a readiness failure. */
export function leaseTone(s: InstanceStatus): Tone {
  if (s.lease === 'disabled') return 'grey';
  return s.lease === 'held' ? 'ok' : 'amber';
}

/** Never backed up, a stale backup, an un-truncated WAL or an interrupted
 * snapshot holding space are each worth an amber tile. */
export function backupTone(s: InstanceStatus, now: Date): Tone {
  const last = parseDate(s.last_backup_at);
  if (!last || now.getTime() - last.getTime() > WEEK_MS) return 'amber';
  if (s.last_backup_wal === 'busy' || s.incomplete_snapshots > 0) return 'amber';
  return 'ok';
}
