// Small shared display components: format tags, permission chips, skeletons.

import { For, Show } from 'solid-js';
import type { EffectivePermission, PermissionFlags } from '../core/types.ts';

/** Ecosystem "container label" — npm / cargo / oci / go. */
export function FormatTag(props: { format: string }) {
  const known = ['npm', 'cargo', 'oci', 'go'];
  const cls = () => (known.includes(props.format) ? `tag tag-${props.format}` : 'tag');
  return <span class={cls()}>{props.format}</span>;
}

export function VisibilityChip(props: { visibility: string }) {
  return (
    <span class={`chip ${props.visibility === 'public' ? 'chip-info' : 'chip-neutral'}`}>
      {props.visibility}
    </span>
  );
}

export function RepoTypeChip(props: { type: string }) {
  return <span class="chip chip-neutral">{props.type}</span>;
}

/** Compact read/write/delete/admin chips. */
export function PermChips(props: { perms: PermissionFlags | EffectivePermission }) {
  const flags = () => [
    ['read', props.perms.can_read] as const,
    ['write', props.perms.can_write] as const,
    ['delete', props.perms.can_delete] as const,
    ['admin', props.perms.can_admin] as const,
  ];
  return (
    <span class="perm-row">
      <For each={flags()}>{([label, on]) => <span class={`perm ${on ? 'on' : ''}`}>{label}</span>}</For>
    </span>
  );
}

/** Table-shaped loading placeholder. */
export function TableSkeleton(props: { rows?: number; cols?: number }) {
  const rows = () => Array.from({ length: props.rows ?? 5 });
  const cols = () => Array.from({ length: props.cols ?? 4 });
  return (
    <div class="table-card">
      <table class="table">
        <tbody>
          <For each={rows()}>
            {(_, r) => (
              <tr>
                <For each={cols()}>
                  {(_, c) => (
                    <td>
                      <div
                        class="skeleton skeleton-text"
                        style={{
                          width: `${[72, 38, 54, 30][(r() + c()) % 4]}%`,
                          'min-width': '36px',
                        }}
                      />
                    </td>
                  )}
                </For>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </div>
  );
}

/** Stat-card loading placeholder grid. */
export function StatsSkeleton() {
  return (
    <div class="stats-grid">
      <For each={[0, 1, 2, 3]}>
        {() => (
          <div class="stat">
            <div class="skeleton skeleton-text" style={{ width: '46%', 'margin-bottom': '14px' }} />
            <div class="skeleton" style={{ width: '58%', height: '30px', 'margin-bottom': '16px' }} />
            <div class="skeleton skeleton-text" style={{ width: '70%' }} />
          </div>
        )}
      </For>
    </div>
  );
}

/** Inline error card with a consistent voice. */
export function LoadError(props: { what: string; detail?: string }) {
  return (
    <div class="alert alert-error" role="alert">
      <div>
        <div style={{ 'font-weight': 600 }}>Couldn't load {props.what}.</div>
        <Show when={props.detail}>
          <div class="small" style={{ opacity: 0.85, 'margin-top': '2px' }}>
            {props.detail}
          </div>
        </Show>
      </div>
    </div>
  );
}
