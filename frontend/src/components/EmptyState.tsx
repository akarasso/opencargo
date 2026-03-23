import { Show } from 'solid-js';
import type { JSX } from 'solid-js';

interface EmptyStateProps {
  title: string;
  text?: string;
  icon?: JSX.Element;
}

export default function EmptyState(props: EmptyStateProps) {
  return (
    <div class="empty-state">
      <Show when={props.icon}>
        <div class="empty-state-icon">{props.icon}</div>
      </Show>
      <div class="empty-state-title">{props.title}</div>
      <Show when={props.text}>
        <div class="empty-state-text">{props.text}</div>
      </Show>
    </div>
  );
}
