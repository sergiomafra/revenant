// The Kit note (Kit.note): a tinted line with an icon. ok, mid (caution), bad, info.
import { CircleCheck, CircleX, Info, TriangleAlert } from 'lucide-react';
import type { ReactNode } from 'react';

export type NoteKind = 'ok' | 'mid' | 'bad' | 'info';
const ICON = { ok: CircleCheck, mid: TriangleAlert, bad: CircleX, info: Info };

export function Note({ kind, children }: { kind: NoteKind; children: ReactNode }) {
  const Icon = ICON[kind];
  return (
    <div className={`note ${kind}`}>
      <Icon className="i" aria-hidden />
      <span>{children}</span>
    </div>
  );
}
