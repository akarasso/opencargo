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

const FOCUSABLE_SELECTOR = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(', ');

// Module-level bookkeeping shared by every mounted dialog: a stack so only
// the topmost modal handles Escape/Tab when modals nest, and a counter so
// the body scroll lock is only released once the last modal closes.
const modalStack: HTMLElement[] = [];
let scrollLockCount = 0;
let previousBodyOverflow = '';

function lockBodyScroll() {
  if (scrollLockCount === 0) {
    previousBodyOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
  }
  scrollLockCount += 1;
}

function unlockBodyScroll() {
  scrollLockCount = Math.max(0, scrollLockCount - 1);
  if (scrollLockCount === 0) document.body.style.overflow = previousBodyOverflow;
}

function focusableIn(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    // Skip elements hidden via display:none etc. — they can't take focus.
    (el) => el.getClientRects().length > 0,
  );
}

export default function Modal(props: ModalProps) {
  return (
    <Show when={props.open}>
      {/* Portal to <body>: a transformed/animated ancestor would otherwise
          become the containing block of this fixed overlay. */}
      <Portal>
        <ModalDialog {...props} />
      </Portal>
    </Show>
  );
}

/** Mounted only while open, so onMount/onCleanup bracket each open/close cycle. */
function ModalDialog(props: ModalProps) {
  // eslint-disable-next-line no-unassigned-vars -- assigned by Solid's ref directive (JSX compiler)
  let dialogRef: HTMLDivElement | undefined;

  function handleKeyDown(e: KeyboardEvent) {
    const dialog = dialogRef;
    // Only the topmost modal reacts; a nested modal swallows the event first.
    if (!dialog || modalStack[modalStack.length - 1] !== dialog) return;

    if (e.key === 'Escape') {
      e.preventDefault();
      props.onClose();
      return;
    }
    if (e.key !== 'Tab') return;

    // Focus trap: wrap Tab/Shift+Tab at the edges, and pull focus back in
    // if it somehow ended up outside the dialog.
    const focusable = focusableIn(dialog);
    if (focusable.length === 0) {
      e.preventDefault();
      dialog.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const active = document.activeElement;
    const activeInside = active instanceof HTMLElement && dialog.contains(active);
    if (e.shiftKey) {
      if (!activeInside || active === first) {
        e.preventDefault();
        last.focus();
      }
    } else if (!activeInside || active === last) {
      e.preventDefault();
      first.focus();
    }
  }

  onMount(() => {
    const dialog = dialogRef;
    if (!dialog) return;
    const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    modalStack.push(dialog);
    lockBodyScroll();
    document.addEventListener('keydown', handleKeyDown, true);

    // Initial focus: first focusable control, otherwise the dialog itself
    // (it carries tabindex="-1" for exactly this case). Deferred a tick so
    // the dialog's children are fully in the DOM.
    queueMicrotask(() => {
      if (!dialog.isConnected) return;
      (focusableIn(dialog)[0] ?? dialog).focus();
    });

    onCleanup(() => {
      document.removeEventListener('keydown', handleKeyDown, true);
      const idx = modalStack.indexOf(dialog);
      if (idx !== -1) modalStack.splice(idx, 1);
      unlockBodyScroll();
      // Hand focus back to whatever opened the modal, if it still exists.
      if (trigger && trigger.isConnected) trigger.focus();
    });
  });

  return (
    <div class="modal-overlay" onClick={() => props.onClose()}>
      <div
        ref={dialogRef}
        class={`modal ${props.wide ? 'modal-wide' : ''}`}
        role="dialog"
        aria-modal="true"
        aria-label={props.title}
        tabindex="-1"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="row" style={{ 'align-items': 'flex-start' }}>
          <div class="grow">
            <div class="modal-title">{props.title}</div>
            <Show when={props.subtitle}>
              <div class="modal-sub">{props.subtitle}</div>
            </Show>
          </div>
          <button class="btn btn-quiet btn-icon" onClick={() => props.onClose()} aria-label="Close">
            <Icon name="x" size={15} />
          </button>
        </div>
        <div style={{ 'margin-top': '10px' }}>{props.children}</div>
        <Show when={props.actions}>
          <div class="modal-actions">{props.actions}</div>
        </Show>
      </div>
    </div>
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
