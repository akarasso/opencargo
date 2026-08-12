import { describe, expect, it } from 'vitest';
import { parsePrometheusMetrics } from './prometheus.ts';

describe('parsePrometheusMetrics', () => {
  it('parses a bare metric family', () => {
    expect(parsePrometheusMetrics('http_requests_total 1027')).toEqual([
      { name: 'http_requests_total', labels: '', value: '1027' },
    ]);
  });

  it('splits labels off the family name', () => {
    expect(parsePrometheusMetrics('http_requests_total{method="post",code="200"} 1027')).toEqual([
      {
        name: 'http_requests_total',
        labels: '{method="post",code="200"}',
        value: '1027',
      },
    ]);
  });

  it('keeps label values containing spaces intact (last-space split)', () => {
    expect(parsePrometheusMetrics('http_route{path="/npm group/foo"} 3')).toEqual([
      { name: 'http_route', labels: '{path="/npm group/foo"}', value: '3' },
    ]);
  });

  it('keeps scientific and special float values as strings', () => {
    const rows = parsePrometheusMetrics(
      ['metric_a 1.7560473e+09', 'metric_b NaN', 'metric_c -42.5'].join('\n'),
    );
    expect(rows.map((r) => r.value)).toEqual(['1.7560473e+09', 'NaN', '-42.5']);
  });

  it('skips comments and blank lines', () => {
    const raw = [
      '# HELP http_requests_total Total requests.',
      '# TYPE http_requests_total counter',
      '',
      '   ',
      'http_requests_total 3',
    ].join('\n');
    expect(parsePrometheusMetrics(raw)).toEqual([
      { name: 'http_requests_total', labels: '', value: '3' },
    ]);
  });

  it('skips malformed lines without a value', () => {
    expect(parsePrometheusMetrics('lonely_token')).toEqual([]);
  });

  it('returns an empty list for empty input', () => {
    expect(parsePrometheusMetrics('')).toEqual([]);
  });

  it('trims surrounding whitespace per line', () => {
    expect(parsePrometheusMetrics('  spaced_metric 7  ')).toEqual([
      { name: 'spaced_metric', labels: '', value: '7' },
    ]);
  });

  it('parses several families in one payload', () => {
    const raw = ['a 1', 'a{l="x"} 2', 'b 3'].join('\n');
    expect(parsePrometheusMetrics(raw).map((m) => m.name)).toEqual(['a', 'a', 'b']);
  });
});
