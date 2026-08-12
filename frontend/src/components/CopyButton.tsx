import { createSignal } from 'solid-js';
import Icon from './Icon.tsx';
import { toasts } from '../core/stores/toasts.ts';

interface CopyButtonProps {
  text: string;
  label?: string;
}

/** Legacy copy path for non-secure origins (plain http) where the Clipboard
 * API is unavailable: select the text in an off-screen textarea and copy. */
function legacyCopy(text: string): boolean {
  const textarea = document.createElement('textarea');
  textarea.value = text;
  // Off-screen but still selectable (display:none would break selection).
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  textarea.style.pointerEvents = 'none';
  document.body.appendChild(textarea);
  textarea.select();
  let ok: boolean;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  textarea.remove();
  return ok;
}

export default function CopyButton(props: CopyButtonProps) {
  const [copied, setCopied] = createSignal(false);

  function markCopied() {
    setCopied(true);
    setTimeout(() => setCopied(false), 1600);
  }

  async function handleCopy() {
    if (navigator.clipboard) {
      try {
        await navigator.clipboard.writeText(props.text);
        markCopied();
        return;
      } catch {
        // Permission denied or insecure context — try the legacy path.
      }
    }
    if (legacyCopy(props.text)) {
      markCopied();
    } else {
      toasts.error('Could not copy to clipboard', 'Select the text and copy it manually.');
    }
  }

  return (
    <button class={`copy-btn ${copied() ? 'copied' : ''}`} onClick={() => void handleCopy()}>
      <Icon name={copied() ? 'check' : 'copy'} size={13} />
      {copied() ? 'Copied' : (props.label || 'Copy')}
    </button>
  );
}
