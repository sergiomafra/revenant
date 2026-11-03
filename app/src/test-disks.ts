// Disk lists for the tests, in the shape `list_disks` returns (crates/revenant-app/src/disks.rs).
import type { DiskView } from './disks/types';

export const SD: DiskView = {
  id: 'disk4',
  name: { label: 'EOS_DIGITAL', vendor: 'SanDisk', model: 'Extreme' },
  size: 64_000_000_000,
  fs: 'exfat',
  connection: 'sd_card',
  kind: 'card',
  solid_state: false,
  camera_card: true,
  selectable: true,
  highlighted: true,
};

export const HD: DiskView = {
  id: 'disk5',
  name: { label: null, vendor: 'LaCie', model: 'Rugged' },
  size: 2_000_000_000_000,
  fs: 'exfat',
  connection: 'usb',
  kind: 'external',
  solid_state: false,
  camera_card: false,
  selectable: true,
  highlighted: true,
};

export const SSD: DiskView = {
  id: 'disk6',
  name: { label: 'T7', vendor: 'Samsung', model: 'PSSD T7' },
  size: 1_000_000_000_000,
  fs: 'exfat',
  connection: 'usb',
  kind: 'external',
  solid_state: true,
  camera_card: false,
  selectable: true,
  highlighted: true,
};

export const SYSTEM: DiskView = {
  id: 'disk0',
  name: { label: 'Macintosh HD', vendor: 'APPLE', model: 'SSD AP0512Q' },
  size: 500_000_000_000,
  fs: 'apfs',
  connection: 'internal',
  kind: 'system',
  solid_state: true,
  camera_card: false,
  selectable: false,
  highlighted: false,
};
