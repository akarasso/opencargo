import { createSignal, For, createRoot } from 'solid-js';
import { Portal } from 'solid-js/web';

type ToastType = 'success' | 'error' | 'info';

interface ToastItem {
  id: number;
  type: ToastType;
  message: string;
}

let nextId = 0;

function createToastStore() {
  const [toasts, setToasts] = createSignal<ToastItem[]>([]);

  function addToast(type: ToastType, message: string, duration = 4000) {
    const id = nextId++;
    setToasts((prev) => [...prev, { id, type, message }]);
    setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
    }, duration);
  }

  function removeToast(id: number) {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }

  return {
    toasts,
    success: (msg: string) => addToast('success', msg),
    error: (msg: string) => addToast('error', msg),
    info: (msg: string) => addToast('info', msg),
    removeToast,
  };
}

export const toast = createRoot(createToastStore);

export default function ToastContainer() {
  return (
    <Portal>
      <div class="toast-container">
        <For each={toast.toasts()}>
          {(t) => (
            <div class={`toast toast-${t.type}`}>
              <span class="toast-message">{t.message}</span>
              <button
                class="toast-close"
                onClick={() => toast.removeToast(t.id)}
              >
                x
              </button>
            </div>
          )}
        </For>
      </div>
    </Portal>
  );
}
