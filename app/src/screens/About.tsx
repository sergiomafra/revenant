// Screen 12 (app.html#tela-12): the logo, the version (the workspace's), the license and the links.
import { openUrl } from '@tauri-apps/plugin-opener';
import { ExternalLink } from 'lucide-react';
import { useId, type MouseEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '../kit/Button';
import { Logo } from '../kit/Logo';
import { Modal } from '../kit/Modal';

// The opener's capability (crates/revenant-app/capabilities/default.json) allows only these addresses.
export const SOURCE_URL = 'https://github.com/sergiomafra/revenant';
export const GUIDE_URL = 'https://github.com/sergiomafra/revenant#readme';

// The webview does not leave the app: the link opens in the system's browser.
function openOutside(e: MouseEvent<HTMLAnchorElement>) {
  e.preventDefault();
  void openUrl(e.currentTarget.href).catch(() => {});
}

export default function About({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const id = useId();
  return (
    <Modal labelledBy={id} onClose={onClose} className="about">
      <Logo />
      <div className="brand" id={id}>
        {t('app.name')}
      </div>
      <div className="ver">{t('about.version', { version: __APP_VERSION__ })}</div>
      <div className="lic">{t('about.license')}</div>
      <div className="links">
        <a href={SOURCE_URL} onClick={openOutside}>
          <ExternalLink className="i" aria-hidden />
          {t('about.source')}
        </a>
        <a href={GUIDE_URL} onClick={openOutside}>
          <ExternalLink className="i" aria-hidden />
          {t('about.guide')}
        </a>
      </div>
      <div className="acts">
        <Button variant="ghost" onClick={onClose}>
          {t('about.close')}
        </Button>
      </div>
    </Modal>
  );
}
