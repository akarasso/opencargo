// ---------------------------------------------------------------------------
// Router search-param helpers.
// ---------------------------------------------------------------------------

/** Normalize a `useSearchParams` value (`string | string[] | undefined`) to a
 * plain string — first entry wins when the param is repeated. */
export function paramStr(val: string | string[] | undefined): string {
  if (Array.isArray(val)) return val[0] ?? '';
  return val ?? '';
}
