import type { JSX } from 'solid-js';
import { Show, onMount, onCleanup } from 'solid-js';

interface ModalProps {
  open: boolean;
  title: string;
  children: JSX.Element;
  actions?: JSX.Element;
  onClose: () => void;
}

export default function Modal(props: ModalProps) {
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') props.onClose();
  }

  onMount(() => {
    document.addEventListener('keydown', handleKeyDown);
  });

  onCleanup(() => {
    document.removeEventListener('keydown', handleKeyDown);
  });

  return (
    <Show when={props.open}>
      <div class="modal-overlay" onClick={props.onClose}>
        <div class="modal" onClick={(e) => e.stopPropagation()}>
          <div class="modal-title">{props.title}</div>
          <div class="modal-body">{props.children}</div>
          <Show when={props.actions}>
            <div class="modal-actions">{props.actions}</div>
          </Show>
        </div>
      </div>
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
          <button class="btn btn-secondary" onClick={props.onCancel}>
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
      {props.message}
    </Modal>
  );
}
