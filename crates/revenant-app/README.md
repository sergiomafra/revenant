# revenant-app

The Revenant desktop app: a [Tauri 2](https://tauri.app/) window around the engine (`revenant-core`). This crate is the
Rust side; the interface (React, Vite, TypeScript, Tailwind) lives in [`app/`](../../app). For now it opens the window
only; the screens and the engine commands come in the next parts.

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

Interface only, without the window: `npm run dev`, `npm test` (Vitest) and `npm run build` (type check, then the bundle
in `app/dist`).

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
- `capabilities/default.json`: what the window may call (Tauri's core defaults only, for now).
- `src/main.rs`: when the system's permission prompt relaunches the binary to open a disk
  (`revenant_core::source::elevated_entry`), it does only that and exits; otherwise it opens the window.

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
