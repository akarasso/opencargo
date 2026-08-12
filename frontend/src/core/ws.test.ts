import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WsEvent } from './types.ts';

type WsModule = typeof import('./ws.ts');

/** Minimal stand-in for the browser WebSocket, driven manually from tests. */
class MockWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: MockWebSocket[] = [];

  readonly url: string;
  readyState: number = MockWebSocket.CONNECTING;
  readonly sent: string[] = [];

  onopen: ((ev: Record<string, never>) => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: ((ev: { code: number }) => void) | null = null;
  onerror: ((ev: Record<string, never>) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = MockWebSocket.CLOSED;
  }

  // -- test drivers ---------------------------------------------------------

  serverOpen(): void {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.({});
  }

  serverSend(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  serverClose(code: number): void {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.({ code });
  }
}

function lastSocket(): MockWebSocket {
  const s = MockWebSocket.instances.at(-1);
  if (!s) throw new Error('no WebSocket was created');
  return s;
}

/** Fresh module instance per test — ws.ts keeps connection state at module
 * scope. Globals must be stubbed BEFORE import (top-level listeners). */
async function loadWs(): Promise<WsModule> {
  vi.resetModules();
  return import('./ws.ts');
}

beforeEach(() => {
  vi.useFakeTimers();
  MockWebSocket.instances = [];
  vi.stubGlobal('WebSocket', MockWebSocket);
  vi.stubGlobal('localStorage', {
    getItem: (): string => 'test-token',
    setItem: (): void => undefined,
    removeItem: (): void => undefined,
  });
  vi.stubGlobal('location', { protocol: 'https:', host: 'registry.test' });
  vi.stubGlobal('window', { addEventListener: (): void => undefined });
  vi.stubGlobal('document', { addEventListener: (): void => undefined, hidden: false });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('reconnectDelayMs', () => {
  it('doubles from 500ms', async () => {
    const { reconnectDelayMs } = await loadWs();
    expect(reconnectDelayMs(0, () => 0.5)).toBe(500);
    expect(reconnectDelayMs(1, () => 0.5)).toBe(1000);
    expect(reconnectDelayMs(2, () => 0.5)).toBe(2000);
    expect(reconnectDelayMs(3, () => 0.5)).toBe(4000);
  });

  it('applies ±25% jitter around the base', async () => {
    const { reconnectDelayMs } = await loadWs();
    expect(reconnectDelayMs(0, () => 0)).toBe(375);
    expect(reconnectDelayMs(0, () => 1)).toBe(625);
  });

  it('caps the base delay at 30s', async () => {
    const { reconnectDelayMs } = await loadWs();
    expect(reconnectDelayMs(20, () => 0.5)).toBe(30_000);
    expect(reconnectDelayMs(20, () => 1)).toBe(37_500); // cap applies pre-jitter
  });

  it('stays inside the jitter band with real randomness', async () => {
    const { reconnectDelayMs } = await loadWs();
    for (let i = 0; i < 200; i++) {
      const d = reconnectDelayMs(3);
      expect(d).toBeGreaterThanOrEqual(3000);
      expect(d).toBeLessThanOrEqual(5000);
    }
  });
});

describe('connection state machine', () => {
  it('authenticates on open and goes online after hello', async () => {
    const ws = await loadWs();
    ws.connectWs();
    expect(ws.wsStatus()).toBe('connecting');
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(lastSocket().url).toBe('wss://registry.test/api/v1/events/ws');

    const connected: WsEvent[] = [];
    ws.onEvent('$connected', (e) => connected.push(e));

    lastSocket().serverOpen();
    expect(JSON.parse(lastSocket().sent[0]) as unknown).toEqual({
      type: 'auth',
      token: 'test-token',
    });

    lastSocket().serverSend({ type: 'hello', username: 'alex' });
    expect(ws.wsStatus()).toBe('online');
    expect(connected).toHaveLength(1);
  });

  it('dispatches domain events to typed and wildcard listeners', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });

    const seen: string[] = [];
    ws.onEvent('package.published', (e) => seen.push(`typed:${e.type}`));
    ws.onEvent('*', (e) => seen.push(`star:${e.type}`));

    lastSocket().serverSend({ type: 'package.published', data: {} });
    expect(seen).toEqual(['typed:package.published', 'star:package.published']);
  });

  it('translates resync frames into $resync', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });

    const resyncs: WsEvent[] = [];
    ws.onEvent('$resync', (e) => resyncs.push(e));
    lastSocket().serverSend({ type: 'resync' });
    expect(resyncs).toHaveLength(1);
  });

  it('ignores frames that are not JSON', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });

    const all: WsEvent[] = [];
    ws.onEvent('*', (e) => all.push(e));
    lastSocket().onmessage?.({ data: 'not json' });
    expect(all).toEqual([]);
    expect(ws.wsStatus()).toBe('online');
  });

  it('onEvent returns a working unsubscribe', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });

    const seen: WsEvent[] = [];
    const unsub = ws.onEvent('cache.updated', (e) => seen.push(e));
    lastSocket().serverSend({ type: 'cache.updated' });
    unsub();
    lastSocket().serverSend({ type: 'cache.updated' });
    expect(seen).toHaveLength(1);
  });

  it('reconnects after an abnormal close', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });
    expect(ws.wsStatus()).toBe('online');

    lastSocket().serverClose(1006);
    expect(ws.wsStatus()).toBe('offline');
    expect(MockWebSocket.instances).toHaveLength(1);

    // hello reset attempts to 0 → first retry delay ∈ [375, 625] ms.
    vi.advanceTimersByTime(626);
    expect(MockWebSocket.instances).toHaveLength(2);
    expect(ws.wsStatus()).toBe('connecting');
  });

  it('backs off across repeated failures', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverClose(1006); // attempt 0 → delay ∈ [375, 625]
    vi.advanceTimersByTime(626);
    expect(MockWebSocket.instances).toHaveLength(2);

    lastSocket().serverClose(1006); // attempt 1 → delay ∈ [750, 1250]
    vi.advanceTimersByTime(700); // below the minimum → no retry yet
    expect(MockWebSocket.instances).toHaveLength(2);
    vi.advanceTimersByTime(600); // past the maximum → retried
    expect(MockWebSocket.instances).toHaveLength(3);
  });

  it('suspends on auth-rejection close 4401 (no retry loop)', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverOpen();
    lastSocket().serverSend({ type: 'hello' });

    lastSocket().serverClose(4401);
    expect(ws.wsStatus()).toBe('offline');
    vi.advanceTimersByTime(120_000);
    expect(MockWebSocket.instances).toHaveLength(1);
  });

  it('suspends on close 4403 as well', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverClose(4403);
    vi.advanceTimersByTime(120_000);
    expect(MockWebSocket.instances).toHaveLength(1);
  });

  it('reconnectWs lifts the suspension (credentials changed)', async () => {
    const ws = await loadWs();
    ws.connectWs();
    lastSocket().serverClose(4401);
    vi.advanceTimersByTime(120_000);
    expect(MockWebSocket.instances).toHaveLength(1);

    ws.reconnectWs();
    expect(MockWebSocket.instances).toHaveLength(2);
    expect(ws.wsStatus()).toBe('connecting');
  });

  it('ignores close events from a superseded socket', async () => {
    const ws = await loadWs();
    ws.connectWs();
    const first = lastSocket();
    ws.reconnectWs(); // supersedes `first`
    expect(MockWebSocket.instances).toHaveLength(2);

    first.serverClose(1006);
    vi.advanceTimersByTime(120_000);
    // The stale close must not schedule extra reconnects for the live socket.
    expect(MockWebSocket.instances).toHaveLength(2);
  });
});
