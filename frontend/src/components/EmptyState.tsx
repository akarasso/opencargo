import { Show } from 'solid-js';
import type { JSX } from 'solid-js';
import Icon from './Icon.tsx';

interface EmptyStateProps {
  title: string;
  text?: string;
  /** Icon name from the inline set (default: package). */
  icon?: string;
  children?: JSX.Element;
}

export default function EmptyState(props: EmptyStateProps) {
  return (
    <div class="empty">
      <Icon name={props.icon ?? 'package'} size={30} strokeWidth={1.4} />
      <div class="empty-title">{props.title}</div>
      <Show when={props.text}>
        <div class="empty-text">{props.text}</div>
      </Show>
      {props.children}
    </div>
  );
}
