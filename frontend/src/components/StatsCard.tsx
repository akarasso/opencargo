import { Show } from 'solid-js';

interface StatsCardProps {
  label: string;
  value: string | number;
  trend?: string;
}

export default function StatsCard(props: StatsCardProps) {
  return (
    <div class="stat-card">
      <div class="stat-card-label">{props.label}</div>
      <div class="stat-card-value">{props.value}</div>
      <Show when={props.trend}>
        <div class="stat-card-trend">{props.trend}</div>
      </Show>
    </div>
  );
}
