# revenant-app

The Revenant desktop app: a [Tauri 2](https://tauri.app/) window around the engine (`revenant-core`). This crate is the
Rust side; the interface (React, Vite, TypeScript, Tailwind) lives in [`app/`](../../app). For now it has the base of
the interface (themes, languages, the Kit, Settings, About and the error screen), the commands that list the disks and
ask for access to one, and the start of the path: choose the disk, give the permission, and a provisional screen that
confirms the access (the search comes in Part 2.2).

## Requirements

- Rust (MSRV 1.88; Tauri is held on the 2.11 line, the newest that builds on 1.88).
- Node 24 and npm. Everything else, the Tauri CLI included, is a local devDependency of `app/`: nothing is installed
  globally.

Install the interface's packages once:

```sh
cd app
npm ci
```

## Develop

```sh
cd app
npm run tauri dev
```

This starts the Vite dev server on `http://localhost:5173` and opens the window on it; changes to the interface reload
live, changes to the Rust side rebuild the app. `npm run tauri -- <command>` runs the Tauri CLI against this crate
(`app/scripts/tauri.mjs` points it here, because the Rust side is not in the usual `app/src-tauri`).

Interface only, without the window: `npm run dev`, `npm test` (Vitest), `npm run check` (the interface rules, below)
and `npm run build` (type check, then the bundle in `app/dist`). Outside the window there is no Rust side: the
commands fail and the settings fall back to the defaults.

## Build the app

```sh
cd app
npm run tauri build -- --bundles app
```

On the Mac this builds the interface, then the release binary, and writes
`target/release/bundle/macos/Revenant.app` (double-click it, or `open` it). There is no `.dmg` and no paid signature in
this part.

`cargo build`, `cargo clippy` and `cargo test` on this crate need `app/dist` to exist, because
`tauri::generate_context!` embeds it: run `npm run build` in `app/` first.

## Configuration

- `tauri.conf.json`: the name (Revenant), the identifier (`com.mafraflow.revenant`), the window (1280×800, at least
  1024×700, title "Revenant") and the bundle. The version comes from the workspace `Cargo.toml`. The test
  `window_config_matches_spec` checks the window.
- `capabilities/default.json`: what the window may call (Tauri's core defaults, and the opener for the two About
  links; the app's own commands need no entry).
- `src/main.rs`: when the system's permission prompt relaunches the binary to open a disk
  (`revenant_core::source::elevated_entry`), it does only that and exits; otherwise it opens the window.

## Themes

Light, dark or automatic (the default, which follows the computer). The colors are the design tokens in
`app/src/styles/tokens.css`, a copy of `docs/superpowers/mockups/m2/tokens.css`: the theme only sets
`data-theme="light"` or `"dark"` on `<html>` (nothing for automatic, where `prefers-color-scheme` decides), exactly
like the mockups.

## Languages

Português (Brasil), Español and English, one catalog each in `app/src/i18n/` (`pt-BR.json`, `es.json`, `en.json`, with
the same keys; the pt-BR texts are the mockups'). The app starts in the computer's language: `pt*` opens in pt-BR,
`es*` in Spanish, anything else in English. In Settings, the language marked "Default" is the computer's one: choosing
it follows the computer again; choosing another keeps that one.

Every error reaches the interface as a code (`{ code: "read_failed" }`); the interface shows `error.<code>.title` and
`error.<code>.action` from the catalog, or `error.unknown` for a code it does not know. To add a code, add both texts to
the three catalogs.

## Settings

The theme and the language are kept in `settings.json` in the app's config folder (Tauri's `app_config_dir`):
`~/Library/Application Support/com.mafraflow.revenant/` on the Mac, `%APPDATA%\com.mafraflow.revenant\` on Windows.

```json
{ "theme": "dark", "language": "es" }
```

`theme` is `light`, `dark` or `auto`; `language` is `pt-BR`, `es`, `en` or `null` (follow the computer). A missing or
unreadable file gives the defaults (`auto`, `null`); deleting it resets the settings. The interface reads and writes it
through two commands, `get_settings` and `set_settings` (`src/settings.rs`); when writing fails, `set_settings` returns
`{ code: "settings_write_failed" }` and the screen says the choice lasts until the app closes.

## Commands: disks and access

Both run off the window thread (`async` plus `spawn_blocking`). Every error is `{ code }`: the engine's `code()`
(`system_tool_failed`, `permission_denied`, `device_not_found`, `system_disk`, `not_whole_disk`, ...), or `unknown`
when the background task itself fails (a panic).

- `list_disks() -> DiskView[]` (`src/disks.rs`): the connected whole disks. Disk images the system attached are left
  out. Order: cards and external disks first (`highlighted`), then internal ones, then the system disk; inside each
  group, the engine's order. A `DiskView` is `{ id, name: { label, vendor, model }, size, fs, connection, kind,
  solid_state, camera_card, selectable, highlighted }`: `kind` is `card`, `external`, `internal` or `system`;
  `connection` is `usb`, `sd_card`, `thunderbolt`, `internal` or `unknown`; `camera_card` is true when a mounted
  volume has a `DCIM` folder; `selectable` is false only for the system disk. `label` and `fs` come from the first
  volume that has one, the EFI system partition left out (its `EFI` label says nothing about the disk). The rule
  matches the volume label only: an EFI partition without a label (common on Windows) can still give a FAT32 tag. The interface
  builds the display name (label; else brand and model; else "Disk" and the size).
- `request_access(disk_id) -> { id, size }` (`src/access.rs`): refuses the system disk (`system_disk`) and a disk
  that is not listed (`device_not_found`), then shows the system's own permission prompt (macOS password dialog,
  Windows UAC), reads the first sector to prove the access works, checks the disk is still listed (the prompt can
  stay open for a long time) and releases the access. A disk gone during that read (`device_gone`) is answered as
  `device_not_found`, so the interface goes back to the list with its note. It never writes. The interface asks again for the actual scan.

Tests use a fake `DeviceOps` and an in-memory device: they never touch a real disk.

## The path so far

`app/src/App.tsx` routes the screens; Settings and About open from the two buttons at the right of every header.

1. **Choose the disk** (screen 1, `screens/ChooseDisk.tsx`, mockup `inicio.html#tela-1`), the first screen. It calls
   `list_disks` on opening and again every 2 seconds (`disks/useDisks.ts`; the next call waits for the answer). The
   cards redraw only when the list changed, and the chosen disk stays chosen while it is listed (when it leaves, the
   choice is cleared). Each card shows the name (`disks/diskName.ts`), the size (powers of 1000, as disks are sold),
   the file system tag, and a line with the brand and "looks like a camera card" or how the disk is connected; the
   icon is a card for `card`, the USB drive for an external SSD, a hard drive otherwise. The system disk is greyed out
   with the reason and cannot be chosen. When a disk that can be chosen is an SSD, the caution note shows, and choosing
   the SSD opens its "Learn why" (Continue still works). No disk to choose: "Connect the card or the disk". The first
   listing failing goes to screen 13 with its code; a later failure keeps the last list and tries again on the next
   round.
2. **Permission** (screen 3, `screens/Permission.tsx`, `inicio.html#tela-3`): the Mac or the Windows text, from
   `navigator.userAgent`/`navigator.platform` (`platformOf`). The button calls `request_access`; while the system
   prompt is open the button is busy and the rest of the window still answers. `permission_denied` shows the "Refused"
   state with Try again (which asks again); `device_not_found` goes back to the list with the note "The disk was not
   found"; any other code goes to screen 13 (Try again there returns to the Permission screen, where the user asks again).
   Only one prompt runs at a time, also after Back and Continue while a prompt is still open.
3. **Access granted** (`screens/AccessGranted.tsx`, provisional, no mockup): "Revenant can read <name> (<size>)" and
   "The search comes in the next version", with Back to the list. Part 2.2 replaces it with the pre-check.

## Content security policy

`tauri.conf.json` sets a strict policy: everything from the app itself, inline styles allowed (React sets style
attributes), images from the app, `data:` and `asset:`, and the commands' channel; no remote code and no `eval`.
Tauri adds a nonce or hash to `style-src` only for `<style>` elements in the built HTML (which would make browsers
ignore `'unsafe-inline'`); the Vite build has none (`app/dist/index.html` links one CSS file), the screens style with
classes, and the one `style` the interface sets (the logo's CSS variables) is written by React through the CSSOM,
which the policy does not block.
`devCsp` adds what the Vite dev server needs (inline scripts for hot reload, its address and WebSocket). The test
`csp_is_strict` checks the release policy.

## Interface rules (`npm run check`)

`scripts/check-app.mjs` (run by `npm run check` in `app/` and by CI) fails when, in `app/src` (tests and
`tokens.css` aside):

- a color is written literally (`#hex`, `rgb(`/`hsl(` and friends, a CSS color name in a color property, a Tailwind
  palette class such as `bg-white`) instead of a token;
- there is an emoji (the catalogs included);
- a visible text is written in JSX (a text node with letters, a string literal child, or `aria-label`, `title`,
  `placeholder`, `alt` with a literal): every text comes from the catalogs;
- a catalog key is missing in another language, or a `t('…')` uses a key that pt-BR does not have;
- `--brand-gold` appears outside `tokens.css` and `kit/Logo.tsx`.

It parses TSX with rolldown's parser from `app/node_modules`, so run `npm ci` first.

## The Kit

`app/src/kit/` holds the components of the mockup kit (`docs/superpowers/mockups/m2/kit.css`, `kit.js`): `Button`,
`Card`, `Note`, `Logo`, `TechDetails` ("Technical details", the only place for codes and jargon), `Modal` and `Page`
(the header with the step trail, the content and the footer with the actions; the buttons at the right of the header
come from the `HeaderActions` context, which the app shell fills with Settings and About). `kit/icons.tsx` draws the
Lucide icons that `lucide-react` no longer ships (the SD card), with Lucide's geometry. Their styles are in
`kit/kit.css`, ported from the mockups' CSS; screen styles are in `src/screens/screens.css` (11 to 13) and
`src/screens/path.css` (1, 3 and the provisional screen). The screens: `ChooseDisk` (1), `Permission` (3),
`AccessGranted` (provisional), `Settings` (11), `About` (12) and `UnexpectedError` (13). The About links open in the browser through `tauri-plugin-opener`; the
capability allows only those two addresses.

## Icons

`icons/` holds the generated icons, checked in. To regenerate them from the brand masters in `assets/brand/` (run in
`app/`):

```sh
npm run tauri -- icon ../assets/brand/app-icon-1024.png -o ../crates/revenant-app/icons
rm -r ../crates/revenant-app/icons/android ../crates/revenant-app/icons/ios
npm run tauri -- icon ../assets/brand/app-icon-flat-1024.png -o /tmp/revenant-flat
cp /tmp/revenant-flat/icon.ico /tmp/revenant-flat/Square*Logo.png /tmp/revenant-flat/StoreLogo.png ../crates/revenant-app/icons/
cp ../assets/brand/app-icon-32.png ../crates/revenant-app/icons/32x32.png
node scripts/small-icons.mjs ../crates/revenant-app/icons/icon.ico ../assets/brand/app-icon-16.png ../assets/brand/app-icon-32.png
node scripts/small-icons.mjs ../crates/revenant-app/icons/icon.icns ../assets/brand/app-icon-16.png ../assets/brand/app-icon-32.png
```

The macOS master has the shadowed tile; Windows (`icon.ico` and the `Square*Logo.png` tiles) uses the flat one. There
are no mobile targets, so the `android/` and `ios/` folders are dropped. At 16 and 32 px the hand-tuned PNGs (thicker
strokes, see `assets/brand/README.md`) replace what `tauri icon` scales down: `32x32.png` and the 16/32 px images
inside `icon.ico` and `icon.icns`.
