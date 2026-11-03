// Screen 1, Choose the disk (inicio.html#tela-1): the connected disks, kept up to date, in the mockup's three states
// (the list; no disk; an SSD chosen, with the caution open). The system disk is shown greyed with the reason.
import { Check, ChevronRight, HardDrive, Info, Usb } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { EngineErrorView } from '../bridge';
import { diskName, diskSubtitle, fsLabel, sizeText } from '../disks/diskName';
import type { DiskView } from '../disks/types';
import { useDisks } from '../disks/useDisks';
import { Button } from '../kit/Button';
import { Card } from '../kit/Card';
import { SdCard } from '../kit/icons';
import { Note } from '../kit/Note';
import { Page } from '../kit/Page';

/** What the screen says above the list when it opens again: the chosen disk was not found. */
export type ChooseDiskNotice = 'disk_gone';

type Props = {
  onContinue: (disk: DiskView) => void;
  /** The first listing failed (screen 13 shows the code). */
  onError: (error: EngineErrorView) => void;
  /** The disk chosen before (when coming back), kept if it is still listed. */
  initialSelected?: string | null;
  notice?: ChooseDiskNotice | null;
  /** How often the list is asked again, in milliseconds. */
  intervalMs?: number;
};

/** The mockup's icons: a card, an external SSD (the USB one), any other disk (a hard drive). */
function DiskIcon({ disk }: { disk: DiskView }) {
  if (disk.kind === 'card') return <SdCard className="i" aria-hidden />;
  if (disk.kind === 'external' && disk.solid_state) return <Usb className="i" aria-hidden />;
  return <HardDrive className="i" aria-hidden />;
}

function Tag({ fs }: { fs: string | null }) {
  const label = fsLabel(fs);
  return label ? <span className="tg">{label}</span> : null;
}

function StopUsing({ disk }: { disk: DiskView | null }) {
  const { t } = useTranslation();
  const noun = disk && disk.kind !== 'card' ? 'disk' : 'card';
  return (
    <div className="hint-wrap">
      <Note kind="info">
        <span className="hint">
          <b>{t(`disks.hint_${noun}`)}</b> {t(`disks.hint_${noun}_rest`)}
        </span>
      </Note>
    </div>
  );
}

export default function ChooseDisk({ onContinue, onError, initialSelected = null, notice = null, intervalMs }: Props) {
  const { t } = useTranslation();
  const { disks, selected, select, error } = useDisks(intervalMs, initialSelected);
  const [whyOpen, setWhyOpen] = useState(false);

  useEffect(() => {
    if (error) onError(error);
  }, [error, onError]);

  const choose = (d: DiskView) => {
    select(d.id);
    setWhyOpen(d.solid_state);
  };

  const pickable = disks?.filter((d) => d.selectable) ?? [];
  const system = disks?.filter((d) => !d.selectable) ?? [];
  const footer = (
    <>
      <span className="t">{t('footer.nothing_changes')}</span>
      <span className="sp" />
      <Button disabled={!selected} onClick={() => selected && onContinue(selected)}>
        {t('disks.continue')}
        <ChevronRight className="i" aria-hidden />
      </Button>
    </>
  );
  const gone = notice === 'disk_gone' ? <Note kind="mid">{t('disks.gone')}</Note> : null;

  let body = null;
  if (disks && pickable.length === 0) {
    body = (
      <>
        {gone}
        <div className="empty">
          <span className="tile big">
            <SdCard className="i" aria-hidden />
          </span>
          <h2 className="ttl">{t('disks.empty_title')}</h2>
          <p className="lead">{t('disks.empty_lead')}</p>
        </div>
        <StopUsing disk={null} />
      </>
    );
  } else if (disks) {
    body = (
      <>
        <div>
          <h2 className="ttl">{t('disks.title')}</h2>
          <p className="lead sub">{t('disks.lead')}</p>
        </div>
        {gone}
        <div className="disks" role="radiogroup" aria-label={t('disks.list_label')}>
          {pickable.map((d) => {
            const on = selected?.id === d.id;
            return (
              <button
                key={d.id}
                type="button"
                role="radio"
                aria-checked={on}
                className={on ? 'card disk sel' : 'card disk'}
                onClick={() => choose(d)}
              >
                <div className="top">
                  <span className="tile">
                    <DiskIcon disk={d} />
                  </span>
                  <Tag fs={d.fs} />
                </div>
                <div>
                  <b>{t('disk.named', { name: diskName(d, t), size: sizeText(d.size) })}</b>
                  <small>{diskSubtitle(d, t)}</small>
                </div>
                <span className="pick">
                  <Check className="i" aria-hidden />
                  {t('disk.chosen')}
                </span>
              </button>
            );
          })}
        </div>
        {system.map((d) => (
          <Card key={d.id} off className="sys">
            <span className="tile">
              <HardDrive className="i" aria-hidden />
            </span>
            <div>
              <b>{diskName(d, t)}</b>
              <small>{t('disk.system')}</small>
            </div>
            <Tag fs={d.fs} />
          </Card>
        ))}
        {pickable.some((d) => d.solid_state) ? (
          <>
            <Note kind="mid">
              {t('disks.ssd_caution')}{' '}
              <button type="button" className="lnk" aria-expanded={whyOpen} onClick={() => setWhyOpen((o) => !o)}>
                {t('disks.why')}
              </button>
            </Note>
            {whyOpen ? (
              <div className="why">
                <Info className="i" aria-hidden />
                <span>{t('disks.why_text')}</span>
              </div>
            ) : null}
          </>
        ) : null}
        <StopUsing disk={selected} />
      </>
    );
  }

  return (
    <Page step="disk" footer={footer}>
      <div className="wrap ini fill">{body}</div>
    </Page>
  );
}
