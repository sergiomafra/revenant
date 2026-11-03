import '@testing-library/jest-dom/vitest';
import { clearMocks } from '@tauri-apps/api/mocks';
import { cleanup } from '@testing-library/react';
import { afterEach, beforeEach } from 'vitest';
import i18n from './i18n';

// jsdom has no ResizeObserver; Radix measures with it.
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

// Every test starts in pt-BR (the mockups' language) with the automatic theme.
beforeEach(async () => {
  await i18n.changeLanguage('pt-BR');
  document.documentElement.removeAttribute('data-theme');
});

afterEach(() => {
  cleanup();
  clearMocks();
});
