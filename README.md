# scar

scar unpacks and repacks Apple `Assets.car` files.

Note: Most of this was made with LLMs, so don't expect great support, but you're free to fork it of course.

Every iOS and macOS app ships its images, icons, and colors inside a compiled asset catalog called `Assets.car`. Apple provides no tool to open one up, edit what's inside, and put it back together. scar does exactly that:

- **Extract** every image, icon, color, and vector from a `.car` file into ordinary PNG and SVG files you can open anywhere.
- **Edit** those files in any image editor.
- **Rebuild** a working `.car` that Apple's own tools accept, with only your edits changed — everything you didn't touch is preserved byte-for-byte.
- **Create** a brand-new catalog from a plain folder of PNGs, or **clone** an existing asset under a new name (the building block for alternate app icons).

scar is written in pure Rust with no dependency on Apple frameworks or tools, so it runs the same on macOS and Linux.

## Install

```sh
cargo build --release
./target/release/scar --help
```

Requires Rust 1.87+. Nothing else — no Xcode, no system libraries.

## Quick start: edit an image inside a .car

```sh
# 1. Unpack the catalog into a folder
scar decompile Assets.car --out extracted/

# 2. Edit any PNG under extracted/renditions/ or extracted/previews/
#    (open it in Preview, Photoshop, GIMP, ...)

# 3. Rebuild
scar compile extracted/ --out Assets.car
```

That's it. scar notices which files you changed and re-encodes only those; every untouched asset goes back in byte-identical. A rebuild with _no_ edits produces a catalog whose every asset is byte-for-byte identical to the original.

## Commands

### `scar info` — see what's inside

```sh
scar info Assets.car              # summary: version, counts, key format
scar info Assets.car --renditions # list every individual asset
```

### `scar decompile` — unpack

```sh
scar decompile Assets.car --out extracted/
scar decompile Assets.car --out extracted/ --raw   # no decoding, raw payloads only
```

Produces:

```
extracted/
  manifest.json   Catalog metadata and per-asset details (edit with care)
  renditions/     Decoded bitmaps as PNG files
  previews/       PNGs for assets scar can decode but not fully re-encode,
                  and editable crops of packed-atlas "link" assets
  data/           SVGs, PDFs, and other raw data assets
  rawpayload/     Formats scar can't decode — kept verbatim so nothing is lost
                  (embedded JPEG/HEIF/PDF files get their real extension)
```

### `scar compile` — repack

```sh
scar compile extracted/ --out rebuilt.car
```

Edited PNGs are re-encoded into the catalog; unedited assets pass through untouched. Two conveniences happen automatically:

- **Color promotion.** If a grayscale-stored asset is edited with color pixels, scar upgrades its storage format to full color instead of silently flattening your edit to gray (it prints a notice when it does).
- **Atlas paste-back.** Many small icons are crops of a larger packed "atlas" image. Their crops appear in `previews/`; edit one and scar pastes it back into the atlas at the right spot on compile. The edit must keep the exact same pixel size — scar never resamples. (The stored crop rects use a bottom-up y origin, CoreGraphics-style; scar handles the flip on both the crop and the paste.)

### `scar pack` — build a catalog from scratch

```sh
scar pack MyIcons/ --out catalog/     # a folder of PNGs and/or *.imageset bundles
scar compile catalog/ --out Assets.car
```

Names, scales (`@2x`, `@3x`), and idioms are inferred from filenames or from `*.imageset` `Contents.json` files; sizes and pixel formats come from the images themselves. `--platform` and `--platform-version` set the deployment target (defaults: `ios`, `15.0`).

### `scar clone-asset` — duplicate an asset under a new name

```sh
scar decompile Assets.car --out extracted/
scar clone-asset extracted/ --from AppIcon --to AppIcon-Alt --image alt-1024.png
scar compile extracted/ --out rebuilt.car
```

Copies the named asset and every rendition that belongs to it (images, multisize entries, links) under a fresh identity — which is exactly what an alternate app icon is. `--image` installs your PNG into every cloned bitmap whose size matches it exactly; other sizes keep the original art with a warning (run the command's edit step per size, or edit the clone's PNGs afterwards — scar does not resample).

## What's supported

Short version: **scar can extract and preserve everything, decode almost everything to viewable PNGs, and re-encode edits for all common formats.** Anything it can't decode is carried through the rebuild untouched, so no catalog is ever damaged by a decompile → compile round trip.

Legend: ✅ full · ⚠️ partial · ❌ not supported. "Decode" = turn into an editable PNG/SVG/data file; "Re-encode" = an _edited_ file is written back in the asset's native format on compile.

| Asset kind                                    | Decode          | Re-encode edits                                 |
| --------------------------------------------- | --------------- | ----------------------------------------------- |
| LZFSE / zlib / uncompressed / chunked bitmaps | ✅              | ✅                                              |
| RLE bitmaps                                   | ✅              | ✅ native (GA8); ARGB edits transcode to LZFSE³ |
| deepmap2 palette                              | ✅              | ✅ native (lossless ≤256 colors)                |
| deepmap2 default (BGRA / grayscale)           | ✅              | ✅ native                                       |
| deepmap2 tiled / chunked                      | ✅              | ⚠️ re-encoded as plain LZFSE                    |
| deepmap2 lossless                             | ✅              | ⚠️ re-encoded as plain LZFSE                    |
| deepmap2 16-bit wide (WBGR/GA16)              | ⚠️ ±1 ¹         | ❌ preserved verbatim                           |
| Wide-gamut WBGR (RGBA16F, LZFSE)              | ✅              | ✅ ²                                            |
| SVG / raw data (incl. LZFSE-wrapped)          | ✅              | ✅                                              |
| Embedded JPEG / HEIF / PNG / PDF              | ✅ extracted    | verbatim round-trip                             |
| Colors, incl. system-color references         | ✅              | ✅                                              |
| Linear gradients (ARGG)                       | ✅              | ✅                                              |
| Packed-atlas links (INLK)                     | ✅ crop preview | ✅ paste-back                                   |
| Multisize image sets (MSIS)                   | ✅              | ✅                                              |
| ASTC / HEVC / other exotic codecs             | ❌              | preserved verbatim                              |

¹ Validated within ±1 per channel against Apple's own decoder; preview-only. Heavily saturated Display-P3 photos can drift ~3/channel because CoreUI applies an extra color-management step scar deliberately omits.
² Edits are 8-bit and get promoted back into the 16-bit float container.
³ CUICatalog garbles natively-encoded ARGB RLE streams, so color RLE edits — including grayscale RLE icons promoted by a color edit — are written as plain LZFSE bitmaps instead (valid, just larger).

Container structures (the BOM tree, headers, key formats, appearance and localization tables) are fully read and written.

## How faithful is it?

- Decompile → compile → decompile is byte-identical across **every** `Assets.car` on a stock macOS install (565/565 system catalogs, including the 152 MB SF Symbols catalog; ~1200 catalogs counting apps and simulator runtimes — 0 differences, 0 errors).
- Rebuilt catalogs are accepted by Apple's own `assetutil` with the same asset census as the original.
- Pixel decoders are validated against a CoreUI-based reference renderer (`cargo run --example cuidump`), not just against scar's own round trip — so "decoded correctly" means "matches what Apple's frameworks draw."
- Unedited images keep their exact original bytes on rebuild (semi-transparent edges are never disturbed by a decode/re-encode cycle).

## Testing

The test corpus has three tiers:

- **Apple-derived fixtures** (`tests/fixtures/`, `tests/re_fixtures/`, `tests/re_catalogs/`, `tests/re_refs/`) — renditions, catalogs, and CoreUI reference render dumps extracted from Apple software. These are the ground truth for byte-exact round-trips and pixel-exact decoding. **They are the property of Apple Inc.** and are not covered by this project's license — see [tests/APPLE_ASSETS_NOTICE.md](tests/APPLE_ASSETS_NOTICE.md).
- **Hand-synthesized renditions** (`tests/hand_synth.rs`) — the exotic types no public tool emits (native RLE, ARGG gradients, wide-gamut bitmaps, …), authored at test time with scar's own writers and validated by Apple's `assetutil`.

Tests that need a fixture tier skip gracefully when it is absent.

## Format documentation

The `Assets.car` format is undocumented; scar's docs are written from empirical reverse engineering and verified against real catalogs:

- [docs/FORMAT.md](docs/FORMAT.md) — the complete binary format: BOM container, headers, rendition layouts, and every payload type.
- [docs/deepmap2.md](docs/deepmap2.md) — Apple's proprietary deepmap2 image codec (containers, tiling, all four sub-codecs, and scar's encoders).

## Licence

scar

Copyright (C) 2026 thea

This program is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.

```
SPDX-License-Identifier: AGPL-3.0-or-later
```
