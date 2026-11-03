// The Revenant symbol (Kit.logo), from assets/brand. `small` is the version drawn for 16-32 px; `onTile` the colors on
// the app-icon tile. The only component allowed to use --brand-gold (scripts/check-app.mjs).
import type { CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';

type Props = { small?: boolean; onTile?: boolean; className?: string };
const gold = { stroke: 'var(--d)' };

export function Logo({ small, onTile, className }: Props) {
  const { t } = useTranslation();
  const style: CSSProperties & { '--d': string } = onTile
    ? { '--d': 'var(--tile-gold)', color: 'var(--tile-ink)' }
    : { '--d': 'var(--brand-gold)' };
  return (
    <svg
      className={['logo', className].filter(Boolean).join(' ')}
      role="img"
      aria-label={t('app.name')}
      style={style}
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 100 100"
      fill="none"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {small ? (
        <>
          <path d="M19 33 A36 36 0 1 0 47 14.2" strokeWidth="9" />
          <polyline points="57 5 47 14 57 23" strokeWidth="9" style={gold} />
          <g transform="translate(50 51) scale(.56) translate(-52 -51)" strokeWidth="17">
            <path d="M32 84 V18 H52 A16 16 0 0 1 52 50 H32" />
            <path d="M54 56 L70 84" style={gold} />
          </g>
        </>
      ) : (
        <>
          <path d="M17.1 31 A38 38 0 1 0 48 12.05" strokeWidth="6" />
          <polyline points="56 4 48 12 56 20" strokeWidth="6" style={gold} />
          <g transform="translate(50 51) scale(.58) translate(-52 -51)" strokeWidth="12">
            <path d="M32 42 V18 H52 A16 16 0 0 1 52 50 H44" />
            <path d="M32 62 V82" />
            <path d="M58 62 L72 84" style={gold} />
          </g>
        </>
      )}
    </svg>
  );
}
