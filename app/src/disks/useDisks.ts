// The connected disks, kept up to date: `list_disks` on opening and then every `intervalMs` (the next call waits for
// the answer, so calls never pile up). The state changes only when the list did (no redraw, no flicker); the chosen
// disk stays chosen while it is listed. A failure after the first answer keeps the last list and tries again on the
// next round; a failure of the first call is returned as `error` (the caller shows screen 13).
import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke, type EngineErrorView } from '../bridge';
import type { DiskView } from './types';

export type Disks = {
  /** null until the first answer. */
  disks: DiskView[] | null;
  selected: DiskView | null;
  select: (id: string) => void;
  error: EngineErrorView | null;
};

export function useDisks(intervalMs = 2000, initialSelected: string | null = null): Disks {
  const [disks, setDisks] = useState<DiskView[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(initialSelected);
  const [error, setError] = useState<EngineErrorView | null>(null);
  const last = useRef<string | null>(null);

  useEffect(() => {
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const round = async () => {
      try {
        const list = await invoke<DiskView[]>('list_disks');
        if (!live) return;
        const key = JSON.stringify(list);
        if (key !== last.current) {
          last.current = key;
          setDisks(list);
          // A disk that left is no longer chosen, even if it comes back later.
          setSelectedId((id) => (list.some((d) => d.id === id && d.selectable) ? id : null));
        }
      } catch (e) {
        if (!live) return;
        if (last.current === null) {
          setError(e as EngineErrorView);
          return; // the error screen takes over; it starts again when the screen opens again
        }
      }
      timer = setTimeout(() => void round(), intervalMs);
    };
    void round();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [intervalMs]);

  const selected = disks?.find((d) => d.id === selectedId && d.selectable) ?? null;
  const select = useCallback((id: string) => setSelectedId(id), []);
  return { disks, selected, select, error };
}
