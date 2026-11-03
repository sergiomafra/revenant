// The stored settings and the two commands that read and write them (crates/revenant-app/src/settings.rs).
import { invoke } from '../bridge';
import type { Language } from '../i18n';

export type Theme = 'light' | 'dark' | 'auto';
/** `language: null` follows the system's language. */
export type Settings = { theme: Theme; language: Language | null };

export const DEFAULT_SETTINGS: Settings = { theme: 'auto', language: null };

/** The stored settings; the defaults if they cannot be read. */
export async function loadSettings(): Promise<Settings> {
  try {
    const s = await invoke<Partial<Settings>>('get_settings');
    return { ...DEFAULT_SETTINGS, ...s };
  } catch {
    return DEFAULT_SETTINGS;
  }
}

/** Stores the settings; rejects with `{ code }` when they cannot be written. */
export function saveSettings(settings: Settings): Promise<void> {
  return invoke<void>('set_settings', { settings });
}
