import { For, Show, onCleanup, onMount } from 'solid-js';
import Icon from './Icon.tsx';
import { toasts, type ToastKind } from '../core/stores/toasts.ts';
import { onEvent } from '../core/ws.ts';

const KIND_ICON: Record<ToastKind, string> = {
  success: 'check-circle',
  error: 'alert-circle',
  info: 'info',
  event: 'zap',
};

export default function Toaster() {
  // Surface real-time registry activity as quiet notifications. The server
  // already scopes who receives which event, so anything arriving here is
  // safe to show.
  onMount(() => {
    const unsubs = [
      onEvent('package.published', (ev) => {
        const d = ev.data ?? {};
        toasts.event(
          `${d.package ?? 'package'} ${d.version ?? ''} published`,
          `to ${d.repository ?? 'registry'}`,
        );
      }),
      onEvent('package.promoted', (ev) => {
        const d = ev.data ?? {};
        toasts.event(
          `${d.package ?? 'package'} ${d.version ?? ''} promoted`,
          `${d.from ?? '?'} → ${d.to ?? '?'}`,
        );
      }),
    ];
    onCleanup(() => unsubs.forEach((u) => u()));
  });

  return (
    <div class="toaster" role="status" aria-live="polite">
      <For each={toasts.toasts()}>
        {(t) => (
          <div class={`toast toast-${t.kind} ${t.leaving ? 'leaving' : ''}`}>
            <Icon name={KIND_ICON[t.kind]} size={16} />
            <div class="grow">
              <div class="toast-title">{t.title}</div>
              <Show when={t.detail}>
                <div class="toast-detail">{t.detail}</div>
              </Show>
            </div>
            <button class="toast-close" onClick={() => toasts.dismiss(t.id)} aria-label="Dismiss">
              <Icon name="x" size={13} />
            </button>
          </div>
        )}
      </For>
    </div>
  );
}
