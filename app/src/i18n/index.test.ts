import { describe, expect, it } from 'vitest';
import { initialLanguage } from './index';

describe('initialLanguage', () => {
  it.each([
    ['pt-BR', 'pt-BR'],
    ['pt-PT', 'pt-BR'],
    ['pt', 'pt-BR'],
    ['es-MX', 'es'],
    ['es', 'es'],
    ['en-GB', 'en'],
    ['de-DE', 'en'],
    ['', 'en'],
  ])('%s -> %s', (nav, want) => {
    expect(initialLanguage(nav)).toBe(want);
  });
});
