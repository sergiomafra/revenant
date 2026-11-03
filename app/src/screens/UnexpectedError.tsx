// Screen 13 (app.html#tela-13): the default for every error code without a screen of its own. The code's title and
// what to do come from the catalog (error.unknown for codes it does not know); the code itself only in the closed
// technical details.
import { Info, RefreshCw, TriangleAlert } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { EngineErrorView } from '../bridge';
import { errorKey } from '../i18n';
import { Button } from '../kit/Button';
import { Page } from '../kit/Page';
import { TechDetails } from '../kit/TechDetails';

type Props = { error: EngineErrorView; onHome?: () => void; onRetry?: () => void };

export default function UnexpectedError({ error, onHome, onRetry }: Props) {
  const { t } = useTranslation();
  const key = errorKey(error.code);
  return (
    <Page footer={<span className="t">{t('footer.nothing_changed')}</span>}>
      <div className="wrap center">
        <span className="tile mid big">
          <TriangleAlert className="i" aria-hidden />
        </span>
        <h2 className="ttl">{t(`${key}.title`)}</h2>
        <p className="lead">{t('error.lead')}</p>
        <div className="err">
          <div className="next">
            <Info className="i" aria-hidden />
            <span>
              <b>{t('error.what_to_do')}</b> {t(`${key}.action`)}
            </span>
          </div>
          <TechDetails code={error.code} />
        </div>
        {onHome || onRetry ? (
          <div className="acts-row">
            {onHome ? (
              <Button variant="ghost" onClick={onHome}>
                {t('error.home')}
              </Button>
            ) : null}
            {onRetry ? (
              <Button onClick={onRetry}>
                <RefreshCw className="i" aria-hidden />
                {t('error.retry')}
              </Button>
            ) : null}
          </div>
        ) : null}
      </div>
    </Page>
  );
}
