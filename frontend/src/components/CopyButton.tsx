import { createSignal } from 'solid-js';
import Icon from './Icon.tsx';

interface CopyButtonProps {
  text: string;
  label?: string;
}

export default function CopyButton(props: CopyButtonProps) {
  const [copied, setCopied] = createSignal(false);

  function handleCopy() {
    navigator.clipboard
      .writeText(props.text)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1600);
      })
      .catch(() => {
        // Clipboard API unavailable (plain-http origin); nothing to signal.
      });
  }

  return (
    <button class={`copy-btn ${copied() ? 'copied' : ''}`} onClick={handleCopy}>
      <Icon name={copied() ? 'check' : 'copy'} size={13} />
      {copied() ? 'Copied' : (props.label || 'Copy')}
    </button>
  );
}
