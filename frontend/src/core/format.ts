// ---------------------------------------------------------------------------
// Display formatting helpers (pure functions).
// ---------------------------------------------------------------------------

/** Parse a backend timestamp. SQLite stores naive UTC ("YYYY-MM-DD HH:MM:SS");
 * event timestamps are RFC 3339. Returns null when unparseable. */
export function parseDate(value: string | null | undefined): Date | null {
  if (!value) return null;
  const iso = value.includes('T') ? value : value.replace(' ', 'T') + 'Z';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? null : d;
}

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });

/** "just now", "12 minutes ago", "3 days ago", else a short date. */
export function timeAgo(value: string | null | undefined): string {
  const d = parseDate(value);
  if (!d) return '—';
  const diffSec = (d.getTime() - Date.now()) / 1000;
  const abs = Math.abs(diffSec);
  if (abs < 45) return 'just now';
  if (abs < 3600) return rtf.format(Math.round(diffSec / 60), 'minute');
  if (abs < 86_400) return rtf.format(Math.round(diffSec / 3600), 'hour');
  if (abs < 30 * 86_400) return rtf.format(Math.round(diffSec / 86_400), 'day');
  return shortDate(value);
}

export function shortDate(value: string | null | undefined): string {
  const d = parseDate(value);
  if (!d) return '—';
  return d.toLocaleDateString('en-US', { year: 'numeric', month: 'short', day: 'numeric' });
}

export function fullDate(value: string | null | undefined): string {
  const d = parseDate(value);
  if (!d) return '—';
  return d.toLocaleString('en-US', {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

const compact = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });
const plain = new Intl.NumberFormat('en-US');

/** 1234 → "1,234" below 10k, "12.3K" above. */
export function formatNumber(n: number | null | undefined): string {
  if (n == null) return '—';
  return n >= 10_000 ? compact.format(n) : plain.format(n);
}

/** Initials for avatar chips: "alexandre" → "AL". */
export function initials(name: string | null | undefined): string {
  return (name || '?').slice(0, 2).toUpperCase();
}
