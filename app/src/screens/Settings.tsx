// Screen 11 (app.html#tela-11): theme and language. Every change applies at once and is stored for the next opening.
import * as RadioGroup from '@radix-ui/react-radio-group';
import { Check } from 'lucide-react';
import { useId } from 'react';
import { useTranslation } from 'react-i18next';
import { LANGUAGES, type Language } from '../i18n';
import { Button } from '../kit/Button';
import { Modal } from '../kit/Modal';
import { Note } from '../kit/Note';
import { useSettings } from '../settings/SettingsProvider';
import type { Theme } from '../settings/settings';

const THEMES: Theme[] = ['light', 'dark', 'auto'];

export default function Settings({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const id = useId();
  const { settings, systemLanguage, setTheme, setLanguage, saveFailed } = useSettings();
  const shown = settings.language ?? systemLanguage;
  return (
    <Modal labelledBy={`${id}-title`} onClose={onClose} className="set">
      <h3 id={`${id}-title`}>{t('settings.title')}</h3>
      <div className="row">
        <b id={`${id}-theme`}>{t('settings.theme')}</b>
        <RadioGroup.Root
          className="seg"
          orientation="horizontal"
          aria-labelledby={`${id}-theme`}
          value={settings.theme}
          onValueChange={(v) => setTheme(v as Theme)}
        >
          {THEMES.map((th) => (
            <RadioGroup.Item key={th} value={th}>
              {t(`settings.theme_${th}`)}
            </RadioGroup.Item>
          ))}
        </RadioGroup.Root>
        <small>{t('settings.theme_hint')}</small>
      </div>
      <div className="row">
        <b id={`${id}-lang`}>{t('settings.language')}</b>
        <RadioGroup.Root
          className="langs"
          aria-labelledby={`${id}-lang`}
          value={shown}
          // The system's language is the default: choosing it follows the system again (null).
          onValueChange={(v) => setLanguage(v === systemLanguage ? null : (v as Language))}
        >
          {LANGUAGES.map((l) => (
            <RadioGroup.Item key={l} value={l} className="lang">
              <span className="rd">
                <Check className="i" aria-hidden />
              </span>
              <span lang={l}>{t(`languages.${l}`)}</span>
              {l === systemLanguage ? <small>{t('settings.language_default')}</small> : null}
            </RadioGroup.Item>
          ))}
        </RadioGroup.Root>
      </div>
      {saveFailed ? <Note kind="mid">{t('settings.save_failed')}</Note> : null}
      <div className="acts">
        <Button onClick={onClose}>{t('settings.done')}</Button>
      </div>
    </Modal>
  );
}
