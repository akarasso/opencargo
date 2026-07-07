// ---------------------------------------------------------------------------
// Real-time WebSocket client.
//
// One connection per tab, authenticated by first frame (the browser cannot
// set an Authorization header on WebSocket handshakes). Reconnects with
// exponential backoff + jitter, keeps itself honest with an app-level
// ping/pong heartbeat, and exposes a reactive connection status the UI can
// display. Domain code subscribes with `onEvent(type, handler)`.
//
// Synthetic events (never sent by the server):
//   $connected — fired after each successful hello; stores should refetch
//   $resync    — server says we lagged; stores should refetch
// ---------------------------------------------------------------------------

import { createSignal } from 'solid-js';
import { token } from './token.ts';
import type { WsEvent } from './types.ts';

export type WsStatus = 'connecting' | 'online' | 'offline';

const [status, setStatus] = createSignal<WsStatus>('offline');
export { status as wsStatus };

type Handler = (event: WsEvent) => void;

const listeners = new Map<string, Set<Handler>>();

let socket: WebSocket | null = null;
let attempts = 0;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
let lastActivity = 0;
/** Set when the server refuses auth (4401/4403): stop retrying until the
 * credentials change. */
let suspended = false;
let started = false;

const HEARTBEAT_MS = 25_000;
/** No frame at all for this long ⇒ the connection is dead, force-reconnect. */
const STALE_MS = 70_000;

/** Subscribe to an event type ('*' for all). Returns an unsubscribe fn. */
export function onEvent(type: string, handler: Handler): () => void {
  let set = listeners.get(type);
  if (!set) {
    set = new Set();
    listeners.set(type, set);
  }
  set.add(handler);
  return () => set!.delete(handler);
}

function emit(type: string, event: WsEvent): void {
  listeners.get(type)?.forEach((h) => h(event));
  if (type !== '*') listeners.get('*')?.forEach((h) => h(event));
}

function wsUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${proto}://${location.host}/api/v1/events/ws`;
}

/** Start (or restart) the connection. Idempotent per state change. */
export function connectWs(): void {
  started = true;
  suspended = false;
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  teardownSocket();

  setStatus('connecting');
  let ws: WebSocket;
  try {
    ws = new WebSocket(wsUrl());
  } catch {
    scheduleReconnect();
    return;
  }
  socket = ws;

  ws.onopen = () => {
    lastActivity = Date.now();
    ws.send(JSON.stringify({ type: 'auth', token: token() ?? undefined }));
  };

  ws.onmessage = (msg) => {
    lastActivity = Date.now();
    let event: WsEvent;
    try {
      event = JSON.parse(msg.data as string);
    } catch {
      return;
    }
    switch (event.type) {
      case 'hello':
        attempts = 0;
        setStatus('online');
        startHeartbeat();
        emit('$connected', event);
        break;
      case 'pong':
        break; // liveness only
      case 'resync':
        emit('$resync', event);
        break;
      default:
        emit(event.type, event);
    }
  };

  ws.onclose = (e) => {
    if (socket !== ws) return; // superseded by a newer connection
    socket = null;
    stopHeartbeat();
    setStatus('offline');
    // 4401 invalid/missing token, 4403 password change required: retrying
    // with the same credentials would loop forever.
    if (e.code === 4401 || e.code === 4403) {
      suspended = true;
      return;
    }
    scheduleReconnect();
  };

  ws.onerror = () => {
    // onclose follows; nothing to do here.
  };
}

/** Tear down and reconnect — used when auth changes (login/logout). */
export function reconnectWs(): void {
  attempts = 0;
  connectWs();
}

function teardownSocket(): void {
  if (socket) {
    const s = socket;
    socket = null;
    s.onopen = s.onmessage = s.onclose = s.onerror = null;
    try {
      s.close();
    } catch {
      // already closed
    }
  }
  stopHeartbeat();
}

function scheduleReconnect(): void {
  if (!started || suspended || reconnectTimer) return;
  // 0.5s, 1s, 2s … capped at 30s, ±25% jitter.
  const base = Math.min(30_000, 500 * 2 ** attempts);
  const delay = base * (0.75 + Math.random() * 0.5);
  attempts += 1;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectWs();
  }, delay);
}

function startHeartbeat(): void {
  stopHeartbeat();
  heartbeatTimer = setInterval(() => {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    if (Date.now() - lastActivity > STALE_MS) {
      // Dead connection the OS hasn't noticed yet.
      connectWs();
      return;
    }
    try {
      socket.send(JSON.stringify({ type: 'ping' }));
    } catch {
      connectWs();
    }
  }, HEARTBEAT_MS);
}

function stopHeartbeat(): void {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

// Come back fast when the network or the tab does.
window.addEventListener('online', () => {
  if (started && status() === 'offline' && !suspended) reconnectWs();
});
document.addEventListener('visibilitychange', () => {
  if (!document.hidden && started && status() === 'offline' && !suspended) {
    reconnectWs();
  }
});
