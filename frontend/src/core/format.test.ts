import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { formatNumber, fullDate, initials, parseDate, shortDate, timeAgo } from './format.ts';

// Expected strings are computed with the same Intl options as the
// implementation so the assertions hold under any ambient locale.

describe('parseDate', () => {
  it('parses SQLite naive UTC timestamps as UTC', () => {
    const d = parseDate('2026-01-15 10:30:00');
    expect(d?.getTime()).toBe(Date.UTC(2026, 0, 15, 10, 30, 0));
  });

  it('parses RFC 3339 timestamps', () => {
    const d = parseDate('2026-01-15T10:30:00Z');
    expect(d?.getTime()).toBe(Date.UTC(2026, 0, 15, 10, 30, 0));
  });

  it('honours explicit offsets in RFC 3339 timestamps', () => {
    const d = parseDate('2026-01-15T10:30:00+02:00');
    expect(d?.getTime()).toBe(Date.UTC(2026, 0, 15, 8, 30, 0));
  });

  it('returns null for null, undefined and empty input', () => {
    expect(parseDate(null)).toBeNull();
    expect(parseDate(undefined)).toBeNull();
    expect(parseDate('')).toBeNull();
  });

  it('returns null for unparseable input', () => {
    expect(parseDate('not a date')).toBeNull();
    expect(parseDate('2026-13-45 99:99:99')).toBeNull();
  });
});

describe('timeAgo', () => {
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-06-15T12:00:00Z'));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("returns 'just now' under 45 seconds, past or future", () => {
    expect(timeAgo('2026-06-15T11:59:30Z')).toBe('just now');
    expect(timeAgo('2026-06-15T12:00:30Z')).toBe('just now');
  });

  it('switches to minutes at 45 seconds', () => {
    expect(timeAgo('2026-06-15T11:59:15Z')).toBe(rtf.format(-1, 'minute'));
  });

  it('formats minutes below one hour', () => {
    expect(timeAgo('2026-06-15T11:48:00Z')).toBe(rtf.format(-12, 'minute'));
  });

  it('formats hours below one day', () => {
    expect(timeAgo('2026-06-15T09:00:00Z')).toBe(rtf.format(-3, 'hour'));
  });

  it('formats days below thirty days', () => {
    expect(timeAgo('2026-06-12T12:00:00Z')).toBe(rtf.format(-3, 'day'));
  });

  it('falls back to a short date beyond thirty days', () => {
    expect(timeAgo('2026-01-15 10:30:00')).toBe(shortDate('2026-01-15 10:30:00'));
  });

  it("returns '—' when unparseable", () => {
    expect(timeAgo(null)).toBe('—');
    expect(timeAgo('nope')).toBe('—');
  });
});

describe('shortDate / fullDate', () => {
  it('renders using the ambient locale', () => {
    const d = new Date(Date.UTC(2026, 0, 15, 10, 30));
    expect(shortDate('2026-01-15 10:30:00')).toBe(
      d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }),
    );
    expect(fullDate('2026-01-15T10:30:00Z')).toBe(
      d.toLocaleString(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
      }),
    );
  });

  it("returns '—' for invalid input", () => {
    expect(shortDate(null)).toBe('—');
    expect(shortDate('bogus')).toBe('—');
    expect(fullDate(undefined)).toBe('—');
    expect(fullDate('bogus')).toBe('—');
  });
});

describe('formatNumber', () => {
  const plain = new Intl.NumberFormat(undefined);
  const compact = new Intl.NumberFormat(undefined, {
    notation: 'compact',
    maximumFractionDigits: 1,
  });

  it("returns '—' for null and undefined", () => {
    expect(formatNumber(null)).toBe('—');
    expect(formatNumber(undefined)).toBe('—');
  });

  it('formats zero and small numbers plainly', () => {
    expect(formatNumber(0)).toBe(plain.format(0));
    expect(formatNumber(1234)).toBe(plain.format(1234));
    expect(formatNumber(9999)).toBe(plain.format(9999));
  });

  it('switches to compact notation at 10 000', () => {
    expect(formatNumber(10_000)).toBe(compact.format(10_000));
    expect(formatNumber(12_345)).toBe(compact.format(12_345));
  });

  it('keeps negative numbers plain (below the compact threshold)', () => {
    expect(formatNumber(-5)).toBe(plain.format(-5));
    expect(formatNumber(-123_456)).toBe(plain.format(-123_456));
  });
});

describe('initials', () => {
  it('takes the first two characters uppercased', () => {
    expect(initials('alexandre')).toBe('AL');
    expect(initials('bob')).toBe('BO');
  });

  it('falls back to ? for empty-ish input', () => {
    expect(initials(null)).toBe('?');
    expect(initials(undefined)).toBe('?');
    expect(initials('')).toBe('?');
  });

  it('handles single-character names', () => {
    expect(initials('x')).toBe('X');
  });
});
