# Revenant brand assets

The logo is a ring with a turning arrow (the "undo": the file comes back) and an R made of three pieces inside
(the broken file that returns whole). Gold appears in two places only: the arrowhead and the leg of the R.

## Colors

| Use | Main | Gold | Background |
| --- | --- | --- | --- |
| App icon (tile) | `#f6f3ec` | `#e9c27a` | `#2f5d50` |
| On a light background | `#2f5d50` | `#b9822a` | `#f6f3ec` |
| On a dark background | `#7fb7a4` | `#e2b45e` | `#17161a` |

The gold is brand only (logo, icon, About screen). It is never a status color; the amber of "Partial" is a different token.

## Files

- `symbol.svg`, `symbol-small.svg`: the drawing only. The main color is `currentColor`; the gold is
  `stroke: var(--d, #b9822a)`, so the app can override it with `--d`.
- `app-icon.svg` / `app-icon-1024.png`: macOS icon (824 px tile centered in 1024, standard shadow). Master for the Mac.
- `app-icon-flat.svg` / `app-icon-flat-1024.png`: same tile filling the whole frame, no shadow. Master for Windows.
- `app-icon-32.png`, `app-icon-16.png`: flat tile with the small symbol, to replace what `tauri icon` generates at those sizes.
- `wordmark-light.svg`, `wordmark-dark.svg`: symbol + "Revenant", transparent background.

## Sizes

Use `symbol.svg` from 48 px up. Below that (32 and 16 px) use `symbol-small.svg`: thicker strokes and a one-piece R,
because the gaps vanish under 1 px.

## Regenerating

`bash scripts/export-brand.sh` re-renders the PNGs from the SVGs with headless Google Chrome and checks each PNG's size
and alpha channel with `sips` (macOS). Set `CHROME` to use another Chrome binary.

The wordmark text uses the system font stack (SF Pro on the Mac, Segoe UI on Windows), so its exact look depends on the
machine that renders it.
