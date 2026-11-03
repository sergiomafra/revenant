// The Kit button: filled (the main action) or ghost, normal or small. `asChild` styles a link or another element.
import { Slot } from '@radix-ui/react-slot';
import type { ButtonHTMLAttributes } from 'react';

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'ghost';
  size?: 'md' | 'sm';
  asChild?: boolean;
};

export function Button({ variant = 'primary', size = 'md', asChild, className, type, ...rest }: Props) {
  const cls = ['btn', variant === 'ghost' && 'ghost', size === 'sm' && 'sm', className].filter(Boolean).join(' ');
  if (asChild) return <Slot className={cls} {...rest} />;
  return <button type={type ?? 'button'} className={cls} {...rest} />;
}
