import { describe, expect, it } from 'vitest';
import i18n from '../i18n';
import { HD, SD } from '../test-disks';
import { diskName, diskSubtitle, fsLabel, sizeText } from './diskName';

const t = i18n.t.bind(i18n);

describe('diskName', () => {
  it('uses the volume label, else brand and model, else "Disk" and the size', () => {
    expect(diskName(SD, t)).toBe('EOS_DIGITAL');
    expect(diskName(HD, t)).toBe('LaCie Rugged');
    expect(diskName({ ...HD, name: { label: null, vendor: null, model: 'Rugged' } }, t)).toBe('Rugged');
    expect(diskName({ ...HD, name: { label: '  ', vendor: null, model: null } }, t)).toBe('Disco de 2 TB');
  });

  it('writes sizes the way disks are sold, in the language of the screen', () => {
    expect(sizeText(64_000_000_000, 'pt-BR')).toBe('64 GB');
    expect(sizeText(63_864_569_856, 'pt-BR')).toBe('64 GB');
    expect(sizeText(1_500_000_000_000, 'pt-BR')).toBe('1,5 TB');
    expect(sizeText(1_500_000_000_000, 'en')).toBe('1.5 TB');
    expect(sizeText(512_000_000, 'en')).toBe('512 MB');
  });

  it('names the file system the way the mockup tags it', () => {
    expect(fsLabel('exfat')).toBe('exFAT');
    expect(fsLabel('apfs')).toBe('APFS');
    expect(fsLabel('NTFS')).toBe('NTFS');
    expect(fsLabel(null)).toBeNull();
  });

  it('subtitle: brand, then camera card or how it is connected', () => {
    expect(diskSubtitle(SD, t)).toBe('SanDisk Extreme · parece um cartão de câmera');
    expect(diskSubtitle(HD, t)).toBe('LaCie · ligado por USB');
    expect(diskSubtitle({ ...HD, connection: 'sd_card' }, t)).toBe('LaCie · num leitor de cartão');
    expect(diskSubtitle({ ...HD, connection: 'thunderbolt' }, t)).toBe('LaCie · ligado por Thunderbolt');
    expect(diskSubtitle({ ...HD, connection: 'unknown' }, t)).toBe('LaCie');
    expect(diskSubtitle({ ...HD, name: { label: null, vendor: null, model: null } }, t)).toBe('ligado por USB');
  });
});
