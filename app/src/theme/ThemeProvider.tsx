// Applies the theme the way the mockups do: data-theme="light"|"dark" on <html>, or none for automatic (the tokens
// then follow prefers-color-scheme).
import { useLayoutEffect, type ReactNode } from 'react';
import type { Theme } from '../settings/settings';

export function applyTheme(theme: Theme, root: HTMLElement = document.documentElement) {
  if (theme === 'light' || theme === 'dark') root.setAttribute('data-theme', theme);
  else root.removeAttribute('data-theme');
}

export function ThemeProvider({ theme, children }: { theme: Theme; children: ReactNode }) {
  useLayoutEffect(() => applyTheme(theme), [theme]);
  return children;
}
