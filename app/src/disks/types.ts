// The shapes the disk commands return (crates/revenant-app/src/disks.rs and access.rs).

export type DiskKind = 'card' | 'external' | 'internal' | 'system';

/** The pieces of a disk's name; the interface builds the text (diskName.ts). */
export type DiskName = { label: string | null; vendor: string | null; model: string | null };

/** A disk as `list_disks` gives it. `connection` is the engine's code: usb, sd_card, thunderbolt, internal, unknown. */
export type DiskView = {
  id: string;
  name: DiskName;
  size: number;
  fs: string | null;
  connection: string;
  kind: DiskKind;
  solid_state: boolean;
  camera_card: boolean;
  selectable: boolean;
  highlighted: boolean;
};

/** What `request_access` answers when the system gave the access (already released again). */
export type AccessView = { id: string; size: number };
