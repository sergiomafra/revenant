// "Technical details" (Kit.tech): closed by default; inside, an optional explanation, the error code, and a button
// that copies the code. The only place where engine codes and jargon may appear.
import { Check, Copy } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from './Button';

type Props = { code: string; children?: ReactNode; className?: string };

export function TechDetails({ code, children, className }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const copy = () => {
    void navigator.clipboard?.writeText(code).then(() => setCopied(true), () => {});
  };
  return (
    <details className={['tech', className].filter(Boolean).join(' ')} open={open}>
      <summary
        onClick={(e) => {
          e.preventDefault(); // the state drives `open`, the same in the webview and in tests
          setOpen((o) => !o);
        }}
      >
        {t('tech.summary')}
      </summary>
      {children ? <p>{children}</p> : null}
      <code>{code}</code>
      <Button variant="ghost" size="sm" className="copy" onClick={copy}>
        {copied ? <Check className="i" aria-hidden /> : <Copy className="i" aria-hidden />}
        {copied ? t('tech.copied') : t('tech.copy')}
      </Button>
    </details>
  );
}
