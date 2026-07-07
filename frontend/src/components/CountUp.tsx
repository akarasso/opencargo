import { createEffect, createSignal, onCleanup } from 'solid-js';
import { formatNumber } from '../core/format.ts';
import { prefersReducedMotion } from '../core/stores/ui.ts';

/** Animated number: eases to the new value whenever it changes. */
export default function CountUp(props: { value: number; duration?: number }) {
  const [display, setDisplay] = createSignal(0);
  let raf = 0;
  let current = 0;

  createEffect(() => {
    const target = props.value;
    cancelAnimationFrame(raf);

    if (prefersReducedMotion() || target === current) {
      current = target;
      setDisplay(target);
      return;
    }

    const from = current;
    const delta = target - from;
    const duration = props.duration ?? 750;
    const start = performance.now();

    const tick = (now: number) => {
      const p = Math.min(1, (now - start) / duration);
      const eased = 1 - Math.pow(1 - p, 3);
      current = from + delta * eased;
      setDisplay(Math.round(current));
      if (p < 1) raf = requestAnimationFrame(tick);
      else current = target;
    };
    raf = requestAnimationFrame(tick);
  });

  onCleanup(() => cancelAnimationFrame(raf));

  return <>{formatNumber(display())}</>;
}
