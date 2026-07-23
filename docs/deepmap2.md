# deepmap2: Apple's proprietary asset-catalog image codec

> **Summary:** deepmap2 (CELM compressionType 11) is the undocumented codec CoreUI has used for most bitmap assets since Monterey; Apple decodes it with private Accelerate functions (`vImageDeepmap2Decode`) and no public spec exists. This document covers the full format — container, tiling, all four sub-codecs, and the 16-bit wide variants — reverse-engineered from disassembly and validated against CoreUI's own rendering (the `cuidump` example).

scar's current support:

| Sub-codec                   | Decode          | Encode                                       |
| --------------------------- | --------------- | -------------------------------------------- |
| 1 None                      | ✅ exact        | — (transcode to LZFSE on edit)               |
| 2 Default, 8-bit GA8/BGRA   | ✅ byte-exact   | ✅ native (`deepmap_encode::encode_default`) |
| 2 Default, 16-bit GA16/RGBW | ✅ ±1 vs oracle | ❌ verbatim passthrough                      |
| 3 Lossless                  | ✅ exact        | — (transcode to LZFSE on edit)               |
| 4 Palette                   | ✅ byte-exact   | ✅ native (`deepmap_encode::encode_palette`) |
| tiling + KCBC chunking      | ✅              | encoders emit single-tile, non-chunked       |

Decoder: `src/deepmap.rs`. Encoders: `src/deepmap_encode.rs`.

## 1. Container framing

> **Summary:** A deepmap2 payload is the normal CELM bitmap envelope, then a small wrapper, then a "dmp2" blob with a 16-byte header. Two header bytes are traps: byte 4 (not byte 7) selects the codec, and byte 6 is a constant to ignore — the real pixel format is byte 7 (and for 8-bit formats, the CSI header's pixelFormat).

```
"MLEC"  u32 flags  u32 compressionType(=11)  u32 field3(payload bytes)
  wrapper: u32 version(=1)  u32 encoding  u64 dmpLen
  dmp2 blob (dmpLen bytes):
    [0:4]   "dmp2"
    [4]     codec          1=None  2=Default  3=Lossless  4=Palette
    [5]     blobVersion    (== 1; readers reject >= 2)
    [6]     innerEncoding  (always 0x0a — ignore)
    [7]     pixelFormat    2=GA8(2 B/px)  4=BGRA(4 B/px)
                           18=GA16(4 B/px)  20=RGBW(8 B/px)
    [8:10]  u16 tileW
    [10:12] u16 tileH
    [12:16] u32 tile-0 compressed length
    [16:..] tile data (see §2)
```

- Wrapper `encoding` mirrors the pixel-format/codec slot ids (2 or 4 in the observed corpus); it is not otherwise load-bearing.
- Full bytes-per-pixel table by pixelFormat code: 1=G8, 2=GA8, 3=RGB8, 4=BGRA8; 17–20 = 2, 4, 6, 8 B/px (G16, GA16, RGB16, RGBW). CoreUI FourCC → code: `'ARGB'→4, 'GA8 '→2, 'GA16'→18, 'RGBW'→20`.

## 2. Chunking and tiling

> **Summary:** Large images are subdivided twice, independently: the CELM layer can split the payload into KCBC "bands" of rows, and within one blob the image splits into vertical tiles, each an independent LZFSE stream. Both must be handled or big images (app icons, wallpapers) fail.

- **Chunking** — when CELM `flags` bit 0 is set, the payload is `field3` KCBC chunks: each is `[20-byte KCBC header][independent dmp2 wrapper+blob]`. The KCBC header's `[12:16]` is the band's **row count**, `[16:20]` its byte length. Bands stack top-to-bottom and must sum to the image height.
- **Tiling** — within one blob, `tileW/tileH` come from the header. `ComputeTileSize` caps a tile's raw size at ~2 MiB, so in practice `tileW = imageW` and tiling is vertical-only. Tiles are row-major, each preceded by a u32 length (tile 0's length is the header's `[12:16]`), each an independent LZFSE stream decoded into its sub-rectangle at the image row stride. Edge tiles clip to the image bounds.

## 3. Codecs 1 (None) and 3 (Lossless)

> **Summary:** The simple cases — the tile is just packed pixels, raw or LZFSE-compressed.

The tile's LZFSE stream (Lossless) or raw bytes (None) inflate to `tileW·tileH·bpp` packed **premultiplied** pixels, unpadded rows, copied to the destination sub-rect and converted to straight RGBA like any CELM bitmap (`codec::raw_to_rgba`; wide formats go through `widegamut::to_rgba`).

## 4. Codec 2 (Default) — planar planes + per-row predictor

> **Summary:** The interesting codec. Each tile stores an alpha plane, one predictor selector per row, and 16-bit residuals split into high/low planes. Pixels are reconstructed by a PNG-like predictor, and color images additionally go through a reversible YCoCg-R transform.

### 4.1 Plane layout (per tile, after LZFSE)

With `ch` = channels (1 gray, 3 color), the tile inflates to `align16(w·h [alpha] + h [selectors] + 2·ch·w·h)` bytes:

```
[ alpha     : w·h    bytes — raw, read straight through (NOT predicted) ]
[ selectors : h      bytes — one predictor id per row (0..4) ]
[ hi plane  : ch·w·h bytes — high residual byte; ch channels INTERLEAVED
                             per pixel ([Y,Co,Cg, Y,Co,Cg, …]), row-major ]
[ lo plane  : ch·w·h bytes — low byte + sign, same interleave ]
```

Per element, `res16 = (hi << 8) | lo`, decoded to a signed delta by a sign-magnitude fold whose sign bit is the **LSB**:

```
delta = (res16 & 1) ? -(res16 >> 1) : (res16 >> 1)
```

### 4.2 Predictors (16-bit values, element stride = ch)

Selectors: `0=None 1=Paeth 2=Left 3=Up 4=Mean`. "Left" is the same channel of the previous pixel (`i − ch`); "up" is the same element one row above.

- Left: `v[i] = delta + v[i−ch]`. Up: `v[i] = delta + up[i]`.
- Mean (`DeepmapUnpredictMean`): first pixel = up; else `delta + ((left + up + 1) >> 1)` — but note the reconstruction uses C division, i.e. truncation toward **zero** (matters for the encoder, §7.2).
- Paeth (`DeepmapUnpredictPaeth`) is **not** PNG-Paeth: it is a per-pixel gradient choice made from channel 0 and applied to all channels — use `left` when `|up0 − upleft0| ≤ |left0 − upleft0|`, else `up`; first pixel = up. (Apple's encoder only emits None/Left on row 0, so `up` there is never referenced.)

The gray path (`RowDecodeY00`) is the same machinery with `ch = 1`; the output gray channel is `clamp(value, 0, 255)`, alpha is the raw plane, and no premultiply step is applied (values are already premultiplied).

### 4.3 Color transform — YCoCg-R lifting

The "YCC" in `DeepmapConvertRowA8_YCC16StoRGBA8888` is not a YCbCr matrix; it is the reversible **YCoCg-R** lifting scheme (adds, subtracts, and one shift). With `Y=ch0, C1=ch1, C2=ch2` and chroma shift `s = (blobVersion != 0)` — always 1 for a valid blob, which makes `Co, Cg` even so the halving never rounds:

```
Co = C1 << s;  Cg = C2 << s;  half(x) = x/2 truncated toward zero
t = Y − half(Cg);  G = t + Cg;  B = t − half(Co);  R = B + Co;  A = alpha
```

`ConvertRow` writes `[R,G,B,A]`, but for pixelFormat 4 (BGRA) those bytes land in a BGRA buffer, so the _displayed_ channels swap: with `s = 1` the straight display map is exactly

```
R = Y − C1 − C2    G = Y + C2    B = Y + C1 − C2     (premultiplied)
```

One more trap: the convert **truncates** to 8 bits, it does not clamp — a reconstructed value of −1 renders as 255 (see §7.2).

## 5. Codec 4 (Palette)

> **Summary:** An indexed-color codec: a raw table of up to 257 BGRA entries followed by an LZFSE stream of one byte per pixel.

After the 16-byte dmp2 header:

```
[ paletteCount * 4 bytes : straight (non-premultiplied) BGRA entries ]
[ "bvx2" LZFSE stream    : width*height bytes of u8 indices, row-major ]
```

- `compressedBlock = (entrySize << 16) | (paletteCount − 1)` with entrySize 4. So `0x00040100` → 257 entries (1028 bytes); the palette region length equals the byte offset of the `bvx2` magic exactly.
- Palette entries are **straight** (non-premultiplied) BGRA. Output is `(B,G,R,A) → (R,G,B,A)` with no alpha math.
- Indices are exactly one byte per pixel; ≤256 entries are reachable even when `paletteCount` reads 257 — real Apple palettes always carry one spare trailing entry (load-bearing for the encoder, §7.1).

Aside: the exported `CUIUncompressQuantizedImageData` is **not** this codec's decoder — it handles a separate standalone quantized format. dmp2 palette blobs are decoded by `CUIUncompressDeepmap2ImageData` → `vImageDeepmap2Decode`.

## 6. 16-bit wide formats (RGBW fmt 20, GA16 fmt 18)

> **Summary:** Wide-gamut ("gamut1") renditions reuse the exact same Default machinery; only the final conversion differs — reconstructed values span twice the 8-bit range and are halved for display. Decode is preview-only (±1 vs Apple); scar has no wide encoder, so these pass through verbatim on compile.

Same container, tiling, plane layout, and predictors as §4 (`reconstruct_default_tile` is shared); `wide_default_tile` converts:

- **RGBW (fmt 20, color, ch=3):** identical YCoCg-R inverse (`R=Y+Co−Cg, G=Y+Cg, B=Y−Co−Cg` with `Co=C1, Cg=C2`; the shift and halving cancel), but reconstructed premultiplied values span ~[0, 515], so the 8-bit value is `value >> 1`. Channels are written in **RGBA order** — no BGRA swap, unlike 8-bit fmt 4.
- **GA16 (fmt 18, gray, ch=1):** gray `>> 1` replicated to R=G=B; alpha is the raw 8-bit plane. Every observed GA16 rendition is a pure-alpha mask (gray = 0), so alpha decode is what matters and it is byte-exact.

**Caveat:** on saturated Display-P3 photo content CoreUI applies an extra wide-gamut color-management step (not a plain P3→sRGB matrix) that scar omits; decode drifts mean ~3/channel there, in-gamut content is exact.

Wide **None/Lossless** blobs are RGBA16F half-float and route to `widegamut::to_rgba` (see FORMAT.md §7).

## 7. Encoders

> **Summary:** scar re-encodes edited pixels natively for the two common sub-codecs. CUICatalog enforces framing rules that scar's own (lenient) decoder does not — the rules below are mandatory.

Compile-path selection (`compile.rs::reencode_edited`) for an edited deepmap2 rendition: palette if the edit has ≤256 distinct colors (lossless and smallest), else `encode_default` for BGRA/GA8, else the plain-LZFSE transcode fallback. Unedited renditions always pass through verbatim.

### 7.1 Palette (`encode_palette`)

Emits the §5 layout as a single-tile, non-chunked payload (`compressedBlock = (4 << 16) | (paletteCount − 1)`). Distinct colors are collected in first-appearance order; >256 distinct colors go through median-cut quantization to 256 representatives (nearest-entry mapping by squared RGBA distance) — lossy but edit-acceptable.

Two CoreUI-critical framing rules (CUICatalog renders violations as garbage while `assetutil` still exits 0):

1. **CELM `flags` must be 0.** Writing flags=2 (which some real renditions carry) makes CUICatalog decode the whole image to garbage.
2. **`paletteCount = usedColors + 1`** — exactly one spare, index-unreachable trailing entry (scar appends `00 00 00 00`). A palette whose count equals the used-color count makes CUICatalog render every pixel at the top index as transparent. Every real Apple palette has the spare entry.

Oracle: `cargo run --release --example validate_palette_oracle`.

### 7.2 Default (`encode_default`)

The inverse of §4 for BGRA (ch=3) and GA8 (ch=1): single tile, one LZFSE stream of the `[alpha][selectors][hi][lo]` planar buffer padded to align16, per-row residual-minimizing predictor choice. Two invariants that CUICatalog enforces but a naive inverse violates:

1. **Never emit Mean (selector 4).** `DeepmapUnpredictMean` divides with truncation toward zero while a `>>1` implementation truncates toward −∞; they disagree by 1 whenever `up + left + 1 < 0`, which happens on the signed chroma channels. Candidate set: `{None, Paeth, Left, Up}`; row 0 is restricted to `{None, Left}`, matching Apple's encoder.
2. **Respect truncation, not clamping.** Because the color convert stores the low byte (§4.3), any reconstructed channel outside [0, 255] renders wrapped. The forward YCoCg-R (`R=Y−C1−C2, G=Y+C2, B=Y+C1−C2`; even-`Co` determinant-4 lattice `{B ≡ R mod 2, B+R−2G ≡ 0 mod 4}`) therefore snaps each premultiplied pixel to its nearest **in-gamut** lattice point with a truncation-aware (`rem_euclid 256`) cost over a ±2 search cube. Result: clamp == truncate == identity, reconstruction within ±1/channel. Grayscale colors and GA8 (luma stored verbatim, no transform) are exact; alpha is raw in both.

Native payloads run ~30% smaller than the LZFSE-transcode fallback, close to Apple's own sizes. Oracle: `cargo run --release --example validate_default_oracle`.

## 8. Fixtures and oracle

> **Summary:** Every decode path is pinned by committed fixtures whose reference outputs come from Apple's own code, not from scar.

- `tests/re_fixtures/dmp2pal_*.csi` + `.rgbaref` — palette decode.
- `tests/re_fixtures/dmp2def_*.csi` + `.packedref`/`.rgbaref` — Default.
- `tests/re_fixtures/dmp2bgra_*` — BGRA Default (byte-exact vs cuidump).
- `tests/re_fixtures/wbgr_*`, `ga16_*` — 16-bit wide (±1 vs cuidump).
- `tests/re_catalogs/` — four real macOS catalogs round-tripped end-to-end.

Reference outputs come from the `cuidump` example (`examples/common/cuidump.rs`), which loads CUICatalog and dumps each named image's rendered premultiplied RGBA — the ground-truth oracle for any new bitmap decoder.
