# Application, document and command artwork

The application and document icons are original vector artwork created for this project and
distributed under GPL-3.0-or-later. Internal command glyphs use the official
Lucide icon set under ISC, including its retained MIT notice for Feather-derived
icons. These are separate sources; no icon font is used.

## Application and document icons

| Editable artwork | Bundle resource | Design |
|---|---|---|
| `app-icon.svg` | `ParquetViewer.icns` | A cobalt header and two ink-blue columns form a tilted table on a white tile |
| `csv-document.svg` | `CSVDocument.icns` | Green horizontal records and a CSV label |
| `parquet-document.svg` | `ParquetDocument.icns` | Indigo vertical columns and a PARQUET label |

Each ICNS contains the standard 16, 32, 128, 256 and 512 point sizes at 1× and
2×. The application and document types select their own resource in
`packaging/Info.plist`. The three designs appear together in `app-icon-preview.png`.

The application render, `app-icon-1024.png`, is embedded by `src/icons.rs` for the
About panel. `src/macos_icon.rs` sets the native application icon on the main
thread at startup, including direct executable launches; it does not read the
working directory or alter system icon caches.

Rebuild on macOS with Xcode Command Line Tools:

```sh
scripts/build-icons.sh
```

The script renders each SVG through AppKit using `scripts/render-icon.swift`,
resizes the full-resolution PNG with `sips`, then assembles each iconset with
`iconutil`. The bundle script checks these assets before compiling the
executable so the in-app and packaged icons are built from the same artwork.

## Lucide command glyphs

- Official project: [lucide-icons/lucide](https://github.com/lucide-icons/lucide).
- Pinned release: [1.65.0](https://github.com/lucide-icons/lucide/releases/tag/1.65.0).
- Pinned commit: [`81875c8c42d2320fa5123b37fee3554f4ca2bddc`](https://github.com/lucide-icons/lucide/tree/81875c8c42d2320fa5123b37fee3554f4ca2bddc).
- Source and license checked through the official GitHub repository on 2026-10-31.
- [Complete upstream license](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/LICENSE):
  ISC plus the included Feather-derived MIT notice, reproduced in
  `THIRD_PARTY_NOTICES.md` and packaged with the app.

`commands/*.svg` contains 42 official SVGs, including the statistics and window minimize glyphs.
Only their local filenames differ: every file is byte-identical to its pinned
upstream source, verified against Git's blob SHA-1. They use a 24×24 viewBox,
rounded line caps/joins, and 2px strokes. `src/icons.rs` replaces
`currentColor` with the shared light/dark foreground or the white foreground of
primary actions. Inactive window controls use the same theme's muted color.
Encoded SVG and GPUI image IDs are cached once per glyph/color variant; the
running app needs no asset directory, font or network.

`icons::checkbox(checked, dark)` returns a 17px decorative GPUI selection box.
The checked state uses the theme's blue primary fill and white Lucide check;
the unchecked state uses a clear input border. The owning control retains its
localized visible label, tooltip, enabled state and input behavior.

Commands keep visible text and localized tooltips. GPUI 0.2.2 does not expose an
ARIA/AX label API here; using vector glyphs alone does not establish screen-reader
support. `command-icons-preview.svg` and `command-icons-preview.png` compare
light/dark glyphs and selection states. The application preview is
`app-icon-1024.png`.

The command contact sheet is rendered with librsvg, preserving its SVG paths:

```sh
rsvg-convert packaging/assets/command-icons-preview.svg -o packaging/assets/command-icons-preview.png
```

| Local asset | Official Lucide source |
|---|---|
| `open.svg` | [file-input](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/file-input.svg) |
| `folder.svg` | [folder-open](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/folder-open.svg) |
| `import.svg` | [file-down](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/file-down.svg) |
| `filter.svg` | [list-filter](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/list-filter.svg) |
| `execute.svg` | [circle-play](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/circle-play.svg) |
| `clear.svg` | [eraser](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/eraser.svg) |
| `fields.svg` | [list-checks](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/list-checks.svg) |
| `export.svg` | [file-up](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/file-up.svg) |
| `reload.svg` | [rotate-cw](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/rotate-cw.svg) |
| `settings.svg` | [settings-2](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/settings-2.svg) |
| `info.svg` | [info](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/info.svg) |
| `freeze.svg` | [pin](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/pin.svg) |
| `unfreeze.svg` | [pin-off](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/pin-off.svg) |
| `copy.svg` | [copy](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/copy.svg) |
| `play.svg` | [play](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/play.svg) |
| `pause.svg` | [pause](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/pause.svg) |
| `stop.svg` | [square](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/square.svg) |
| `next.svg` | [skip-forward](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/skip-forward.svg) |
| `back.svg` | [arrow-left](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/arrow-left.svg) |
| `close.svg` | [x](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/x.svg) |
| `search.svg` | [search](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/search.svg) |
| `table.svg` | [table-2](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/table-2.svg) |
| `code.svg` | [code-xml](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/code-xml.svg) |
| `image.svg` | [image](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/image.svg) |
| `audio.svg` | [audio-lines](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/audio-lines.svg) |
| `check.svg` | [check](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/check.svg) |
| `external-link.svg` | [external-link](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/external-link.svg) |
| `sun.svg` | [sun](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/sun.svg) |
| `moon.svg` | [moon](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/moon.svg) |
| `move-left.svg` | [arrow-left-to-line](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/arrow-left-to-line.svg) |
| `move-right.svg` | [arrow-right-to-line](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/arrow-right-to-line.svg) |
| `move-up.svg` | [arrow-up-to-line](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/arrow-up-to-line.svg) |
| `move-down.svg` | [arrow-down-to-line](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/arrow-down-to-line.svg) |
| `expand.svg` | [maximize-2](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/maximize-2.svg) |
| `columns.svg` | [columns-3](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/columns-3.svg) |
| `row-range.svg` | [brackets](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/brackets.svg) |
| `all-rows.svg` | [list](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/list.svg) |
| `calendar.svg` | [calendar-days](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/calendar-days.svg) |
| `read-only.svg` | [lock-keyhole](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/lock-keyhole.svg) |
| `wrap.svg` | [text-wrap](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/text-wrap.svg) |
| `minimize.svg` | [minus](https://github.com/lucide-icons/lucide/blob/81875c8c42d2320fa5123b37fee3554f4ca2bddc/icons/minus.svg) |

