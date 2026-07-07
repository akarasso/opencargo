import type { JSX } from 'solid-js';
import { Show, onMount, onCleanup } from 'solid-js';
import { Portal } from 'solid-js/web';
import Icon from './Icon.tsx';

interface ModalProps {
  open: boolean;
  title: string;
  subtitle?: string;
  wide?: boolean;
  children: JSX.Element;
  actions?: JSX.Element;
  onClose: () => void;
}

export default function Modal(props: ModalProps) {
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape' && props.open) props.onClose();
  }

  onMount(() => {
    document.addEventListener('keydown', handleKeyDown);
  });
  onCleanup(() => {
    document.removeEventListener('keydown', handleKeyDown);
  });

  return (
    <Show when={props.open}>
      {/* Portal to <body>: a transformed/animated ancestor would otherwise
          become the containing block of this fixed overlay. */}
      <Portal>
        <div class="modal-overlay" onClick={props.onClose}>
          <div
            class={`modal ${props.wide ? 'modal-wide' : ''}`}
            role="dialog"
            aria-modal="true"
            aria-label={props.title}
            onClick={(e) => e.stopPropagation()}
          >
            <div class="row" style={{ 'align-items': 'flex-start' }}>
              <div class="grow">
                <div class="modal-title">{props.title}</div>
                <Show when={props.subtitle}>
                  <div class="modal-sub">{props.subtitle}</div>
                </Show>
              </div>
              <button class="btn btn-quiet btn-icon" onClick={props.onClose} aria-label="Close">
                <Icon name="x" size={15} />
              </button>
            </div>
            <div style={{ 'margin-top': '10px' }}>{props.children}</div>
            <Show when={props.actions}>
              <div class="modal-actions">{props.actions}</div>
            </Show>
          </div>
        </div>
      </Portal>
    </Show>
  );
}

interface ConfirmModalProps {
  open: boolean;
  title: string;
  message: string;
  confirmLabel?: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmModal(props: ConfirmModalProps) {
  return (
    <Modal
      open={props.open}
      title={props.title}
      onClose={props.onCancel}
      actions={
        <>
          <button class="btn btn-ghost" onClick={props.onCancel}>
            Cancel
          </button>
          <button
            class={`btn ${props.danger ? 'btn-danger' : 'btn-primary'}`}
            onClick={props.onConfirm}
          >
            {props.confirmLabel || 'Confirm'}
          </button>
        </>
      }
    >
      <p class="muted small" style={{ 'line-height': '1.55' }}>
        {props.message}
      </p>
    </Modal>
  );
}
