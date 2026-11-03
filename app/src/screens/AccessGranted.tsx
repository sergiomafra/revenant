// The provisional screen after the permission (Part 2.1 only; the pre-check replaces it in 2.2): Revenant can read
// the chosen disk, and the search comes in the next version. No mockup of its own: the pieces of screens 3 and 4.
import { Check } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { ownName, sizeText } from '../disks/diskName';
import type { AccessView, DiskView } from '../disks/types';
import { Button } from '../kit/Button';
import { Page } from '../kit/Page';

type Props = { disk: DiskView; access: AccessView; onBack: () => void };

export default function AccessGranted({ disk, access, onBack }: Props) {
  const { t } = useTranslation();
  const name = ownName(disk);
  const size = sizeText(access.size);
  return (
    <Page
      step="permission"
      footer={
        <>
          <Button variant="ghost" onClick={onBack}>
            {t('granted.back')}
          </Button>
          <span className="t">{t('footer.nothing_changed')}</span>
        </>
      }
    >
      <div className="wrap ini narrow center">
        <span className="tile ok big">
          <Check className="i" aria-hidden />
        </span>
        <h2 className="ttl">{name ? t('granted.title', { name, size }) : t('granted.title_unnamed', { size })}</h2>
        <p className="lead">{t('granted.lead')}</p>
      </div>
    </Page>
  );
}
