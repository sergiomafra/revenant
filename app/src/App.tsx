// The app shell: loads the settings (theme and language), puts Settings and About in every header, and routes the
// path: Choose the disk (the first screen) -> Permission -> the provisional AccessGranted screen; screen 13 for any
// error without a screen of its own.
import { Info, Settings as SettingsIcon } from 'lucide-react';
import { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { EngineErrorView } from './bridge';
import type { AccessView, DiskView } from './disks/types';
import { HeaderActions } from './kit/Page';
import About from './screens/About';
import AccessGranted from './screens/AccessGranted';
import ChooseDisk, { type ChooseDiskNotice } from './screens/ChooseDisk';
import Permission, { platformOf } from './screens/Permission';
import Settings from './screens/Settings';
import UnexpectedError from './screens/UnexpectedError';
import { SettingsProvider } from './settings/SettingsProvider';

type Route =
  | { to: 'disks'; selected?: string; notice?: ChooseDiskNotice }
  | { to: 'permission'; disk: DiskView }
  | { to: 'granted'; disk: DiskView; access: AccessView }
  | { to: 'error'; error: EngineErrorView; retry: Route };

const OS = platformOf(navigator.userAgent, navigator.platform);

function HeaderButtons({ open }: { open: (d: 'settings' | 'about') => void }) {
  const { t } = useTranslation();
  return (
    <>
      <button
        type="button"
        className="icb"
        aria-label={t('header.settings')}
        title={t('header.settings')}
        onClick={() => open('settings')}
      >
        <SettingsIcon className="i" aria-hidden />
      </button>
      <button
        type="button"
        className="icb"
        aria-label={t('header.about')}
        title={t('header.about')}
        onClick={() => open('about')}
      >
        <Info className="i" aria-hidden />
      </button>
    </>
  );
}

function Screens() {
  const [route, setRoute] = useState<Route>({ to: 'disks' });
  const [dialog, setDialog] = useState<'settings' | 'about' | null>(null);
  const home = useCallback(() => setRoute({ to: 'disks' }), []);
  const listFailed = useCallback(
    (error: EngineErrorView) => setRoute({ to: 'error', error, retry: { to: 'disks' } }),
    [],
  );

  let screen;
  switch (route.to) {
    case 'disks':
      screen = (
        <ChooseDisk
          initialSelected={route.selected}
          notice={route.notice}
          onContinue={(disk) => setRoute({ to: 'permission', disk })}
          onError={listFailed}
        />
      );
      break;
    case 'permission': {
      const { disk } = route;
      screen = (
        <Permission
          key={disk.id}
          disk={disk}
          os={OS}
          onGranted={(access) => setRoute({ to: 'granted', disk, access })}
          onBack={() => setRoute({ to: 'disks', selected: disk.id })}
          onGone={() => setRoute({ to: 'disks', notice: 'disk_gone' })}
          onError={(error) => setRoute({ to: 'error', error, retry: route })}
        />
      );
      break;
    }
    case 'granted': {
      const { disk, access } = route;
      screen = <AccessGranted disk={disk} access={access} onBack={() => setRoute({ to: 'disks', selected: disk.id })} />;
      break;
    }
    case 'error': {
      const retry = route.retry;
      // After a failed listing, trying again is going home: one button is enough.
      const onHome = retry.to === 'disks' ? undefined : home;
      screen = <UnexpectedError error={route.error} onHome={onHome} onRetry={() => setRoute(retry)} />;
      break;
    }
  }

  return (
    <HeaderActions.Provider value={<HeaderButtons open={setDialog} />}>
      {screen}
      {dialog === 'settings' ? <Settings onClose={() => setDialog(null)} /> : null}
      {dialog === 'about' ? <About onClose={() => setDialog(null)} /> : null}
    </HeaderActions.Provider>
  );
}

export default function App() {
  return (
    <SettingsProvider>
      <div data-testid="app" className="h-full">
        <Screens />
      </div>
    </SettingsProvider>
  );
}
