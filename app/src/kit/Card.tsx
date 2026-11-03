// The Kit card: a surface with a border; `selected` adds the accent ring, `off` the dashed, unavailable look.
import type { HTMLAttributes } from 'react';

type Props = HTMLAttributes<HTMLDivElement> & { selected?: boolean; off?: boolean };

export function Card({ selected, off, className, ...rest }: Props) {
  const cls = ['card', selected && 'sel', off && 'off', className].filter(Boolean).join(' ');
  return <div className={cls} {...rest} />;
}
