// The interface languages: one catalog per language (same keys in all three; scripts/check-app.mjs checks it).
import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import en from './en.json';
import es from './es.json';
import ptBR from './pt-BR.json';

export const LANGUAGES = ['pt-BR', 'es', 'en'] as const;
export type Language = (typeof LANGUAGES)[number];

/** The language to open in, from the system's (`navigator.language`): pt* -> pt-BR, es* -> es, anything else -> en. */
export function initialLanguage(navigatorLanguage: string): Language {
  const primary = navigatorLanguage.toLowerCase().split(/[-_]/)[0];
  if (primary === 'pt') return 'pt-BR';
  if (primary === 'es') return 'es';
  return 'en';
}

i18n.use(initReactI18next).init({
  resources: { 'pt-BR': { translation: ptBR }, es: { translation: es }, en: { translation: en } },
  lng: initialLanguage(typeof navigator === 'undefined' ? '' : navigator.language),
  fallbackLng: 'pt-BR',
  supportedLngs: [...LANGUAGES],
  interpolation: { escapeValue: false }, // React already escapes
  initAsync: false,
});

/** The catalog entry for an engine error code: its own when the catalog has it, `error.unknown` otherwise. */
export function errorKey(code: string): string {
  return i18n.exists(`error.${code}.title`) ? `error.${code}` : 'error.unknown';
}

export default i18n;
