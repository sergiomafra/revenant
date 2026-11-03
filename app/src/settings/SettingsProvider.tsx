// Loads the settings once, applies the theme and the language, and stores every change.
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import i18n, { initialLanguage, type Language } from '../i18n';
import { ThemeProvider } from '../theme/ThemeProvider';
import { DEFAULT_SETTINGS, loadSettings, saveSettings, type Settings, type Theme } from './settings';

type SettingsContextValue = {
  settings: Settings;
  /** The language the system asks for (what `language: null` means). */
  systemLanguage: Language;
  setTheme: (theme: Theme) => void;
  /** `null` follows the system's language again. */
  setLanguage: (language: Language | null) => void;
  /** The last change could not be stored (it still applies until the app closes). */
  saveFailed: boolean;
};

const SettingsContext = createContext<SettingsContextValue | null>(null);

export function useSettings(): SettingsContextValue {
  const v = useContext(SettingsContext);
  if (!v) throw new Error('useSettings outside SettingsProvider');
  return v;
}

function applyLanguage(language: Language) {
  document.documentElement.lang = language;
  if (i18n.language !== language) void i18n.changeLanguage(language);
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const systemLanguage = initialLanguage(navigator.language);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [saveFailed, setSaveFailed] = useState(false);

  useEffect(() => {
    let live = true;
    void loadSettings().then((s) => {
      if (!live) return;
      applyLanguage(s.language ?? systemLanguage);
      setSettings(s);
    });
    return () => {
      live = false;
    };
  }, [systemLanguage]);

  // The latest settings, read by `change` so that the store call stays out of the state updater.
  const current = useRef<Settings | null>(null);
  current.current = settings;
  const change = useCallback(
    (patch: Partial<Settings>) => {
      const next: Settings = { ...(current.current ?? DEFAULT_SETTINGS), ...patch };
      current.current = next;
      if ('language' in patch) applyLanguage(next.language ?? systemLanguage);
      setSettings(next);
      saveSettings(next).then(
        () => setSaveFailed(false),
        () => setSaveFailed(true),
      );
    },
    [systemLanguage],
  );

  // Nothing renders before the settings are known: no flash of the wrong theme or language.
  if (!settings) return null;
  const value: SettingsContextValue = {
    settings,
    systemLanguage,
    setTheme: (theme) => change({ theme }),
    setLanguage: (language) => change({ language }),
    saveFailed,
  };
  return (
    <SettingsContext.Provider value={value}>
      <ThemeProvider theme={settings.theme}>{children}</ThemeProvider>
    </SettingsContext.Provider>
  );
}
