import type { Role } from '../core/types.ts';

const LABELS: Record<string, string> = {
  admin: 'Admin',
  publisher: 'Publisher',
  reader: 'Reader',
  anonymous: 'Anonymous',
};

export default function RoleBadge(props: { role: Role | string }) {
  const cls = () => `role role-${LABELS[props.role] ? props.role : 'reader'}`;
  return <span class={cls()}>{LABELS[props.role] ?? props.role}</span>;
}
