// Screen 3, Permission (inicio.html#tela-3): explains the system's own prompt (the Mac's administrator password or
// Windows' UAC question), then asks for it with `request_access`. Refused or cancelled: the "Refused" state with Try
// again. The disk gone: back to the list (the caller shows the notice). Any other code: screen 13.
import { ChevronRight, LoaderCircle, Lock, RefreshCw } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { invoke, type EngineErrorView } from '../bridge';
import type { AccessView, DiskView } from '../disks/types';
import { Button } from '../kit/Button';
import { Card } from '../kit/Card';
import { Note } from '../kit/Note';
import { Page } from '../kit/Page';
import { TechDetails } from '../kit/TechDetails';

export type Platform = 'mac' | 'windows';

/** Which system's prompt to explain, from `navigator.userAgent` and `navigator.platform` (the Mac when unsure). */
export function platformOf(userAgent: string, platform: string): Platform {
  return /^win/i.test(platform) || /windows/i.test(userAgent) ? 'windows' : 'mac';
}

type Props = {
  disk: DiskView;
  os: Platform;
  onGranted: (access: AccessView) => void;
  onBack: () => void;
  /** The disk is no longer listed (`device_not_found`). */
  onGone: () => void;
  onError: (error: EngineErrorView) => void;
};

const P = { mac: 'permission.mac', windows: 'permission.win' } as const;

export default function Permission({ disk, os, onGranted, onBack, onGone, onError }: Props) {
  const { t } = useTranslation();
  const [denied, setDenied] = useState(false);
  const [busy, setBusy] = useState(false);
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const ask = async () => {
    setBusy(true);
    try {
      const access = await invoke<AccessView>('request_access', { diskId: disk.id });
      if (live.current) onGranted(access);
    } catch (e) {
      if (!live.current) return;
      const err = e as EngineErrorView;
      setBusy(false);
      if (err.code === 'permission_denied') setDenied(true);
      else if (err.code === 'device_not_found') onGone();
      else onError(err);
    }
  };

  const status = busy ? <span className="sr-only" role="status">{t('permission.waiting')}</span> : null;
  const back = (
    <Button variant="ghost" onClick={onBack}>
      {t('permission.back')}
    </Button>
  );

  if (denied) {
    return (
      <Page
        step="permission"
        footer={
          <>
            {back}
            <span className="sp" />
            {status}
            <Button onClick={() => void ask()} disabled={busy} aria-busy={busy}>
              {busy ? <LoaderCircle className="i spin" aria-hidden /> : <RefreshCw className="i" aria-hidden />}
              {t('permission.retry')}
            </Button>
          </>
        }
      >
        <div className="wrap ini narrow center">
          <span className="tile mid big">
            <Lock className="i" aria-hidden />
          </span>
          <h2 className="ttl">{t('permission.denied_title')}</h2>
          <p className="lead">{t('permission.denied_lead')}</p>
          <TechDetails code="permission_denied">{t('permission.denied_tech')}</TechDetails>
        </div>
      </Page>
    );
  }

  const k = P[os];
  return (
    <Page
      step="permission"
      footer={
        <>
          {back}
          <span className="t">{t('permission.private')}</span>
          <span className="sp" />
          {status}
          <Button onClick={() => void ask()} disabled={busy} aria-busy={busy}>
            {busy ? <LoaderCircle className="i spin" aria-hidden /> : null}
            {!busy && os === 'mac' ? <Lock className="i" aria-hidden /> : null}
            {t(`${k}_ask`)}
            {!busy && os === 'windows' ? <ChevronRight className="i" aria-hidden /> : null}
          </Button>
        </>
      }
    >
      <div className="perm">
        <div className="txt">
          <span className="tile">
            <Lock className="i" aria-hidden />
          </span>
          <h2 className="ttl">{t(`${k}_title`)}</h2>
          <p className="lead">{t(`${k}_lead`)}</p>
          <Note kind="info">{t('permission.read_only')}</Note>
        </div>
        <Card className="steps">
          <h3>{t('permission.how')}</h3>
          {[1, 2, 3].map((n) => (
            <div key={n} className={n === 1 ? 'stp on' : 'stp'}>
              <em>{n}</em>
              <span>
                <Trans i18nKey={`${k}_step${n}`} components={{ b: <b /> }} />
              </span>
            </div>
          ))}
        </Card>
      </div>
    </Page>
  );
}
