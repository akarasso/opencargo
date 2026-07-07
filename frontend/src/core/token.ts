// ---------------------------------------------------------------------------
// Auth token holder.
//
// Lives in its own module so http.ts, ws.ts and stores/session.ts can all
// read the token without importing each other (no circular dependencies).
// ---------------------------------------------------------------------------

import { createSignal } from 'solid-js';

const STORAGE_KEY = 'opencargo_token';

const [token, setTokenSignal] = createSignal<string | null>(
  localStorage.getItem(STORAGE_KEY),
);

export { token };

export function setToken(value: string | null): void {
  setTokenSignal(value);
  if (value === null) {
    localStorage.removeItem(STORAGE_KEY);
  } else {
    localStorage.setItem(STORAGE_KEY, value);
  }
}
