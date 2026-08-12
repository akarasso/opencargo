// ---------------------------------------------------------------------------
// Prometheus text exposition format parsing (pure functions).
// ---------------------------------------------------------------------------

export interface ParsedMetric {
  name: string;
  labels: string;
  value: string;
}

/** Parse the Prometheus text format into rows. Comments (`# HELP`, `# TYPE`),
 * blank lines and lines without a value are skipped. */
export function parsePrometheusMetrics(raw: string): ParsedMetric[] {
  const metrics: ParsedMetric[] = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const space = trimmed.lastIndexOf(' ');
    if (space === -1) continue;
    const key = trimmed.slice(0, space);
    const value = trimmed.slice(space + 1);
    const brace = key.indexOf('{');
    metrics.push({
      name: brace === -1 ? key : key.slice(0, brace),
      labels: brace === -1 ? '' : key.slice(brace),
      value,
    });
  }
  return metrics;
}
