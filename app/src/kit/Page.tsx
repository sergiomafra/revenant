// The Kit page: the header (logo, name, on the path screens the step trail, and the app's buttons: Settings and
// About), the scrolling content and the footer with the actions.
import { createContext, useContext, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Logo } from './Logo';

/** The seven steps of the path, in order (the trail in the header). */
export const STEPS = ['disk', 'permission', 'check', 'copy', 'search', 'results', 'recover'] as const;
export type Step = (typeof STEPS)[number];

export function StepTrail({ now }: { now: Step }) {
  const { t } = useTranslation();
  const at = STEPS.indexOf(now);
  return (
    <nav className="path" aria-label={t('steps.label')}>
      {STEPS.map((s, i) => (
        <span
          key={s}
          className={i < at ? 'done' : i === at ? 'now' : undefined}
          aria-current={i === at ? 'step' : undefined}
        >
          {t(`steps.${s}`)}
        </span>
      ))}
    </nav>
  );
}

/** The buttons at the right of every header (the app shell provides them: Settings and About). */
export const HeaderActions = createContext<ReactNode>(null);

export function Header({ step }: { step?: Step }) {
  const { t } = useTranslation();
  const actions = useContext(HeaderActions);
  return (
    <header className="hd">
      <Logo small />
      <span className="name">{t('app.name')}</span>
      {step ? <StepTrail now={step} /> : null}
      {actions ? <div className="hd-acts">{actions}</div> : null}
    </header>
  );
}

export function Footer({ children }: { children: ReactNode }) {
  return <footer className="foot">{children}</footer>;
}

type PageProps = { step?: Step; footer?: ReactNode; children?: ReactNode };

export function Page({ step, footer, children }: PageProps) {
  return (
    <div className="page">
      <Header step={step} />
      <main className="main">
        <div className="content">{children}</div>
      </main>
      {footer ? <Footer>{footer}</Footer> : null}
    </div>
  );
}
