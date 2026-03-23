import type { JSX } from 'solid-js';

type BadgeVariant = 'default' | 'success' | 'warning' | 'danger' | 'purple' | 'orange' | 'mono';

interface BadgeProps {
  variant?: BadgeVariant;
  children: JSX.Element;
}

export default function Badge(props: BadgeProps) {
  const variant = () => props.variant || 'default';
  return (
    <span class={`badge badge-${variant()}`}>
      {props.children}
    </span>
  );
}

export function RoleBadge(props: { role: string }) {
  const variant = (): BadgeVariant => {
    switch (props.role) {
      case 'admin': return 'danger';
      case 'publisher': return 'purple';
      case 'reader': return 'default';
      default: return 'default';
    }
  };
  return <Badge variant={variant()}>{props.role}</Badge>;
}

export function FormatBadge(props: { format: string }) {
  const variant = (): BadgeVariant => {
    switch (props.format) {
      case 'npm': return 'danger';
      case 'cargo': return 'orange';
      default: return 'default';
    }
  };
  return <Badge variant={variant()}>{props.format}</Badge>;
}

export function TypeBadge(props: { type: string }) {
  const variant = (): BadgeVariant => {
    switch (props.type) {
      case 'hosted': return 'success';
      case 'proxy': return 'warning';
      case 'group': return 'purple';
      default: return 'default';
    }
  };
  return <Badge variant={variant()}>{props.type}</Badge>;
}
