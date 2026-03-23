import { createSignal } from 'solid-js';

interface CopyButtonProps {
  text: string;
  label?: string;
}

export default function CopyButton(props: CopyButtonProps) {
  const [copied, setCopied] = createSignal(false);

  function handleCopy() {
    navigator.clipboard.writeText(props.text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }

  return (
    <button
      class={`copy-btn ${copied() ? 'copy-btn-copied' : ''}`}
      onClick={handleCopy}
    >
      {copied() ? 'Copied' : (props.label || 'Copy')}
    </button>
  );
}
