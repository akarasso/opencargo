import { For, Show } from 'solid-js';
import type { JSX } from 'solid-js';
import EmptyState from './EmptyState.tsx';

interface Column<T> {
  header: string;
  accessor: (row: T) => JSX.Element;
  class?: string;
}

interface DataTableProps<T> {
  columns: Column<T>[];
  data: T[];
  emptyTitle?: string;
  emptyText?: string;
}

export default function DataTable<T>(props: DataTableProps<T>) {
  return (
    <Show
      when={props.data.length > 0}
      fallback={
        <div class="data-table-wrapper">
          <EmptyState
            title={props.emptyTitle || 'No data'}
            text={props.emptyText || 'Nothing to display.'}
          />
        </div>
      }
    >
      <div class="data-table-wrapper">
        <table class="data-table">
          <thead>
            <tr>
              <For each={props.columns}>
                {(col) => <th class={col.class}>{col.header}</th>}
              </For>
            </tr>
          </thead>
          <tbody>
            <For each={props.data}>
              {(row) => (
                <tr>
                  <For each={props.columns}>
                    {(col) => <td class={col.class}>{col.accessor(row)}</td>}
                  </For>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </div>
    </Show>
  );
}

export type { Column };
