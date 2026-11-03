// The texts of a disk card: its name, its size, the file system tag and the line under the name.
import type { TFunction } from 'i18next';
import i18n from '../i18n';
import type { DiskView } from './types';

const clean = (s: string | null) => (s && s.trim() ? s.trim() : null);

/** "Brand Model", or whichever of the two the system gave; null without either. */
function brandAndModel(d: DiskView): string | null {
  return [clean(d.name.vendor), clean(d.name.model)].filter(Boolean).join(' ') || null;
}

/** The disk's own name (the volume label, else brand and model), or null when the system gives none. */
export function ownName(d: DiskView): string | null {
  return clean(d.name.label) ?? brandAndModel(d);
}

/** The disk's name: its own one, or "Disk" and the size. */
export function diskName(d: DiskView, t: TFunction): string {
  return ownName(d) ?? t('disk.fallback_name', { size: sizeText(d.size, i18n.language) });
}

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];

/** Bytes the way disks are sold (powers of 1000): "64 GB", "1,5 TB"; one decimal below 10, in `language`. */
export function sizeText(bytes: number, language: string = i18n.language): string {
  let v = bytes;
  let u = 0;
  while (v >= 1000 && u < UNITS.length - 1) {
    v /= 1000;
    u++;
  }
  const n = new Intl.NumberFormat(language, { maximumFractionDigits: v < 10 ? 1 : 0 }).format(v);
  return `${n} ${UNITS[u]}`;
}

const FS: Record<string, string> = {
  exfat: 'exFAT',
  msdos: 'FAT',
  fat: 'FAT',
  fat12: 'FAT',
  fat16: 'FAT',
  fat32: 'FAT32',
  apfs: 'APFS',
  hfs: 'HFS+',
  ntfs: 'NTFS',
  refs: 'ReFS',
};

/** The file system tag (the system's names, macOS and Windows, written the usual way). */
export function fsLabel(fs: string | null): string | null {
  const f = clean(fs);
  if (!f) return null;
  return FS[f.toLowerCase()] ?? f.toUpperCase();
}

const VIA: Record<string, string> = {
  usb: 'disk.via_usb',
  sd_card: 'disk.via_sd_card',
  thunderbolt: 'disk.via_thunderbolt',
};

/** The line under the name: the brand, then "looks like a camera card" or how the disk is connected. */
export function diskSubtitle(d: DiskView, t: TFunction): string {
  // The brand already in the name is not repeated in full: with a label, brand and model; without, only the brand.
  const brand = clean(d.name.label) ? brandAndModel(d) : clean(d.name.vendor);
  const how = d.camera_card ? t('disk.camera_card') : VIA[d.connection] ? t(VIA[d.connection]) : null;
  return [brand, how].filter(Boolean).join(' · ');
}
