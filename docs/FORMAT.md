# The Assets.car binary format

Everything below was empirically verified against real catalogs (CoreUI-974/975 era) and Apple's own tools (`assetutil`, CoreUI rendering via the `cuidump` example). Facts that are inferred but not verified are marked _assumed_.

Byte offsets are from structure start. The outer BOM layer is **big-endian**; everything inside CAR blocks (CARHEADER, KEYFORMAT, CSI headers, payloads) is **little-endian**.

Apple's proprietary deepmap2 image codec has its own document: [deepmap2.md](deepmap2.md).

## 1. BOM container

> **Summary:** A `.car` file is a BOM ("Bill of Materials") archive — an old NeXT-era container format that is basically a tiny filesystem: a table of numbered blocks, a list of named entry points ("vars"), and B-tree structures for key/value lookup. All the actual asset data lives in blocks that the vars and trees point to.

### 1.1 Header (512 bytes, zero-padded)

```
off  size  field
0    8     magic "BOMStore"
8    4     version         (= 1)
12   4     numberOfBlocks  (count of non-null blocks)
16   4     indexOffset
20   4     indexLength     (bytes)
24   4     varsOffset
28   4     varsLength      (bytes)
```

### 1.2 Block index (at indexOffset)

```
u32 count            (may exceed numberOfBlocks; includes null entries)
count * { u32 address, u32 length }   // block id = position in this table
```

- Block id 0 is a null entry (address 0, length 0).
- Some catalogs mark unused slots with `address = length = 0xFFFFFFFF` (a free-slot sentinel). Treat these like empty blocks; referenced blocks are always in-bounds.
- After the entries the index block contains a freelist. When writing, the freelist trailer must be a fixed **20 zero bytes** — writing only a bare 4-byte zero count makes BOM readers fail with `BOMStreamGetDataPointer buffer overflow`.

### 1.3 Vars (at varsOffset)

```
u32 count
count * { u32 blockIndex, u8 nameLength, name bytes (no padding) }
```

Vars seen in real catalogs: `CARHEADER`, `RENDITIONS`, `FACETKEYS`, `APPEARANCEKEYS`, `KEYFORMAT`, `EXTENDED_METADATA`, `BITMAPKEYS`, `LOCALIZATIONKEYS`. (Also known in the wild: `EXTERNAL_KEYS`, `KEYFORMATWORKAROUND`, `CARGLOBALS`, `THUMBNAILS`.) Any var not understood by a writer must still be preserved or its data is silently dropped.

### 1.4 BOM trees

> **Summary:** The lookup structures (`RENDITIONS`, `FACETKEYS`, …) are B-trees: a small "tree" descriptor block points at node blocks holding sorted key→value entries. Keys and values are usually block references, but one tree flavor stores small keys inline. Two details are load-bearing and easy to get wrong: internal nodes use _last_-key separators, and the inline-keys flag must be written correctly.

Tree descriptor block:

```
u32 magic "tree"
u32 version       (= 1)
u32 childBlockId  -> root paths (node) block
u32 nodeSize      (4096 typical; 1024 seen for BITMAPKEYS)
u32 pathCount     (total leaf entries)
u8  isInlineKeys  (0 = keys are block references; 1 = keys are raw inline u32
                   values — the BITMAPKEYS style)
```

On disk the block is often 29 bytes (an 8-byte tail: a `keyLengthHint` u32 — fixed key length, `0xFFFFFFFF` for variable, 0 for inline-key trees — and a reserved u32 = 0). The tail is optional padding real readers tolerate being absent; writing a 21-byte descriptor round-trips fine. What is **not** optional is `isInlineKeys`: writing 0 for an inline-key tree makes BOM readers resolve the raw key value as a block id and fail with `BOMStorageCopyFromBlock: bid > storage->blocks`.

Paths (node) block:

```
u16 isLeaf   (1 = leaf, 0 = internal)
u16 count
u32 forward  (next leaf block id, 0 = none)
u32 backward (prev leaf block id, 0 = none)
count * { u32 valueBlockId, u32 keyBlockId }
```

- **Internal-node separators are the LAST key of each child subtree.** Apple's lookup descends into the first entry whose key compares `>=` the target, so writing first-key separators misroutes every key that doesn't begin a leaf — `assetutil` then reports "can't get size of value" and marks renditions corrupt. The convention only matters once a tree outgrows one leaf (511+ entries at nodeSize 4096), so small catalogs hide the bug.
- Node blocks must be zero-padded up to `nodeSize` bytes on disk even when their content is smaller; tightly-packed nodes trigger the same `BOMStreamGetDataPointer buffer overflow` as a short freelist.
- In an inline-key tree the `keyBlockId` field is not a block id at all — it is the raw u32 key value. Do not resolve it through the index.
- Iterate a tree by descending first entries to the leftmost leaf, then following `forward`.

Sort orders: RENDITIONS keys compare as byte strings (memcmp ascending) of the u16-LE attribute values in keyformat order. FACETKEYS keys are facet name strings, memcmp ascending.

## 2. CARHEADER

> **Summary:** A fixed 436-byte header carrying version strings, a rendition count, and a few IDs. Mostly informational; preserve and update the count.

Magic bytes on disk are "RATC" ('CTAR' read little-endian).

```
off  size field                        typical value
0    4    magic "RATC"
4    4    coreuiVersion                974
8    4    storageVersion               17
12   4    storageTimestamp             0
16   4    renditionCount
20   128  mainVersionString            "@(#)PROGRAM:CoreUI  PROJECT:CoreUI-974.1"
148  256  versionString                "Xcode 26.4 (17E192) ..."
404  16   uuid
420  4    associatedChecksum           0
424  4    schemaVersion                2
428  4    colorSpaceID                 1
432  4    keySemantics                 2
```

## 3. EXTENDED_METADATA

> **Summary:** 1 KiB of fixed-width strings recording how the catalog was built (thinning args, deployment platform/version, authoring tool).

```
0    4    magic "META"
4    1024 four zero-padded char[256] fields:
          thinningArguments, deploymentPlatformVersion ("15.0"),
          deploymentPlatform ("ios"), authoringTool
```

## 4. KEYFORMAT

> **Summary:** Declares which attributes make up a rendition key and in what order. Every RENDITIONS tree key is a list of u16 values in exactly this order, so the keyformat is the decoder ring for the whole catalog.

Magic bytes "tmfk" ('kfmt' little-endian):

```
u32 magic, u32 reserved(0), u32 numTokens, numTokens * u32 attributeId
```

Attribute ids: 0 look, 1 element, 2 part, 3 size, 4 direction, 5 placeholder, 6 value, 7 appearance, 8 dimension1, 9 dimension2, 10 state, 11 layer, 12 scale, 13 localization, 14 presentationState, 15 idiom, 16 subtype, 17 identifier, 18 previousValue, 19 previousState, 20 sizeClassHorizontal, 21 sizeClassVertical, 22 memoryClass, 23 graphicsClass, 24 displayGamut, 25 deploymentTarget.

A typical token order: appearance, localization, scale, idiom, subtype, dimension2, dimension1, sizeClassHorizontal, sizeClassVertical, identifier, element, part (12 tokens → 24-byte keys).

## 5. RENDITIONS: the CSI blob

> **Summary:** Each rendition (one image at one scale/idiom/appearance, one color, one vector, …) is a "CSI" blob: a fixed 184-byte header, then a list of tagged metadata records (TLVs), then a payload whose format depends on the rendition type. The payload's own magic bytes — not the layout number — determine how to parse it.

Key = numTokens u16-LE values in keyformat order. Value = CSI blob.

### 5.1 CSI header (184 bytes, magic "ISTC" = 'CTSI' LE)

```
off  size field
0    4    magic "ISTC"
4    4    version (1)
8    4    renditionFlags (bitfield; observed 0x0, 0x4, 0x8, 0x14 — preserve verbatim)
12   4    width
16   4    height
20   4    scaleFactor (100 = 1x, 200 = 2x)
24   4    pixelFormat (see §5.2)
28   4    colorSpaceID (low byte; observed 0,1,2)
32   4    modificationDate (unix, usually 0)
36   2    layout (see §5.4)
38   2    zero
40   128  name, zero padded ("wallpaper-light@1x.png", "ZZZZPackedAsset-1.1.0-gamut0", …)
168  4    infoListLength (TLV bytes following the header)
172  4    unknownA (=1 everywhere observed)
176  4    unknownB (=0 everywhere observed)
180  4    payloadLength (bytes after the TLV list; == remaining size)
```

### 5.2 Pixel formats

On-disk u32s whose ASCII bytes read:

| bytes                 | meaning                                  |
| --------------------- | ---------------------------------------- |
| `"BGRA"` ('ARGB' LE)  | 8-bit BGRA, premultiplied, 4 B/px        |
| `" 8AG"` ('GA8 ')     | 8-bit gray+alpha, premultiplied, 2 B/px  |
| `"WBGR"` (0x52474257) | wide-gamut RGBA16F, 8 B/px (see §7)      |
| `"61AG"` ('GA16')     | 16-bit gray+alpha unorm, 4 B/px (see §7) |
| `" GVS"` ('SVG ')     | vector (RAWD payload)                    |
| `"ATAD"` ('DATA')     | raw data (RAWD payload)                  |
| `"GEPJ"` ('JPEG')     | embedded JPEG _(assumed)_                |
| 0                     | none: link stubs, MSIS sets, colors      |

An unknown pixel format must degrade to verbatim passthrough, never a hard error, or arbitrary catalogs fail to decompile.

**Stride can contradict the pixel-format name.** Some renditions (observed: layout 1008 "depth gradient" wallpaper strips) declare `BGRA` but carry a TLV 0x3EF bytesPerRow of `width*8` — really a 16-bit, 8 B/px bitmap. Decoding as 4 B/px silently drops half the data and corrupts on re-encode. Rule: only treat a bitmap as decodable when `bytesPerRow == round32(width * bppOf(pixelFormat))`; on mismatch, pass through verbatim.

### 5.3 TLV info list

Repeated `{ u32 tag, u32 length, bytes[length] }` records after the header. Unknown tags must be preserved as raw bytes for round-trip.

```
0x3E9 slices:      u32 nSlices, then per slice 4 u32 (x, y, w, h)
                   (rects round-trip verbatim; if ever *interpreted* as pixel
                   regions, expect the y origin to be bottom-up like INLK's —
                   see §6.4 — not top-down)
0x3EB metrics:     u32 nMetrics, then 3 u32 pairs (edge insets t/l/b/r, image size w,h)
0x3EC composition: u32 blendMode, f32 opacity
0x3ED uti:         UTI string (confirmed: "public.layeredimage" on iconstacks)
0x3EE bitmapInfo:  u32 (exif orientation? =1 everywhere observed)
0x3EF bytesPerRow: u32 (present on CELM bitmaps; width*4 rounded up to 32 for BGRA)
0x3F2 internal link (see INLK, §6.4)
0x3F4 layer descriptors (IconComposer stacks; variable-length, not decoded)
0x3FC named-layer references: { u64 count, count * { u64 flags=1, u32 nameLen,
      name+NUL } } — references sibling renditions by CSI name (icon groups)
0x3FD per-layer f32 array (parallel to 0x3FC; opacity/blend params, assumed)
```

### 5.4 Layout census

> **Summary:** The layout field classifies what the rendition _is_ (image, color, link, gradient, …), but parsing dispatches on the payload magic — most layout numbers share the same payload shapes. This census maps every layout observed in real catalogs to its payload.

Layouts observed in real catalogs:

| Layout     | Payload                  | Meaning                                                        |
| ---------- | ------------------------ | -------------------------------------------------------------- |
| 9          | RAWD                     | vector/SVG (Xcode asset-catalog path)                          |
| 12         | CELM                     | one-part image                                                 |
| 31         | CELM                     | ordinary bitmap, alternate tag (CoreUI-975 authoring path)     |
| 1000       | RAWD (bplist)            | CoreStructuredImage / effect archive (NSKeyedArchiver)         |
| 1003       | none (TLV 0x3F2)         | internal link into a packed atlas                              |
| 1004       | CELM                     | packed atlas image                                             |
| 1007       | RTXT ("RadarHashMap256") | opaque; verbatim passthrough                                   |
| 1008       | CELM                     | depth-gradient wallpaper strip (16-bit despite BGRA tag, §5.2) |
| 1009       | COLR                     | color                                                          |
| 1010       | MSIS                     | multisize image set                                            |
| 1017       | RAWD                     | SVG, IconComposer path (same payload as layout 9)              |
| 1019       | RAWD (empty)             | `.iconstack` layered-image container stub                      |
| 1020       | RAWD (empty)             | icon-group stub; content lives in TLVs 0x3F4/0x3FC/0x3FD       |
| 1021       | ARGG                     | linear gradient (see §6.6)                                     |
| 0–3, 6     | CELM                     | one/three/nine-part variants _(assumed)_                       |
| 20, 24, 30 | —                        | observed only as verbatim passthrough                          |

Layouts 1017/1019/1020/1000/31 need no layout-specific code: dispatch on the payload magic handles them all byte-exact.

## 6. Payload variants

The payload starts at `184 + infoListLength` and is identified by its own 4-byte magic.

### 6.1 CELM bitmaps (magic "MLEC")

> **Summary:** The general bitmap container. A small header says whether the data is one blob or a series of chunks, and which of ~11 compression types encodes the pixels. LZFSE and deepmap2 dominate real catalogs.

```
u32 magic "MLEC", u32 flags, u32 compressionType, u32 lengthOrChunkCount
```

- `flags` bit 0 = chunked (then field 4 is the chunk count, otherwise the payload byte length). **When writing deepmap2, `flags` must be 0** — some real renditions carry flags=2 and still render, but CUICatalog decodes a _written_ palette payload with flags=2 to garbage even though `assetutil` accepts it.
- `compressionType`: 0 uncompressed, 1 rle, 2 zlib, 3 lzvn, 4 lzfse, 5 jpeg-lzfse, 6 blurred, 7 astc, 8 palette-img, 9 hevc, 10 deepmap-lossless, 11 deepmap2.
- Plain (non-chunked): `length` bytes follow — a single LZFSE stream (`bvx2`/`bvxn`…`bvx$`) for lzfse/lzvn, a zlib stream for zlib, or raw premultiplied rows of `bytesPerRow` for uncompressed.
- Chunked: chunkCount × `{ 20-byte header: u32 "KCBC", u32 0, u32 0, u32 rowCount, u32 compressedLength }` + compressedLength bytes (one complete LZFSE stream covering `rowCount * bytesPerRow` bytes).
- deepmap2 (type 11): see [deepmap2.md](deepmap2.md). Fully decoded (all four sub-codecs, tiled and chunked, 8-bit exact and 16-bit wide ±1); scar has native encoders for the palette and default sub-codecs.

Bitmap pixels are premultiplied alpha. `BGRA` byte order in memory; GA8 is 2 bytes/px gray+alpha. bytesPerRow comes from TLV 0x3EF.

#### RLE (compressionType 1)

> **Summary:** A simple per-row run-length codec. The subtle part: rows with identical bytes share one offset — that is deduplication, not an "empty row" marker.

As implemented by CoreUI's `__decompressRLE16`/`__decompressRLE32`:

```
u32 magic(=3), u32 width, u32 height
u32 rowOffset[height]        // byte offset of each row's stream, from body start
... per-row RLE streams ...
```

Each row decodes independently: start at `rowOffset[r]` and stop once exactly `width` elements have been produced (element = 2 B GA8 / 4 B BGRA); there is no explicit end-of-row marker. Control u32 LE: `count = ctrl & 0xFFFFFF`, `flag = ctrl >> 24`; flag 0x80 = fill (read 1 element, repeat count×), 0x00 = literal (count elements inline). (CoreUI's exact predicate: `(int)ctrl > 0x80FFFFFF` ⇒ literal.)

Two rows may share a `rowOffset` when byte-identical — a dedup, **not** an empty-row marker. A naive "equal offsets = transparent row" read decodes some real images wrong (a margin row can share its offset with an opaque fill row elsewhere).

scar decodes and **encodes** RLE natively (`src/rle.rs`): the encoder is greedy (fill for runs ≥ 2, literal otherwise; identical rows dedup), does not reproduce Apple's exact run/literal split, but produces streams CoreUI renders pixel-identically.

### 6.2 RAWD raw data (magic "DWAR")

> **Summary:** The envelope for vectors, plists, and arbitrary data. The "version" word is really a compression flag — writing it wrong makes Apple's tools hang forever.

```
u32 magic "DWAR", u32 version, u32 rawLength, bytes
```

The `version` word is a **compression indicator**: 0 = the bytes are stored verbatim; 1 = they are an LZFSE stream to inflate (SVG text is typically stored this way). **Writing version 1 over non-LZFSE or empty bytes makes CoreUI/assetutil LZFSE-inflate forever** — the streaming decoder never reaches end-of-stream and hangs in an unbounded `reallocf` loop. On encode, always write version 0 for uncompressed data; on decode, sniffing the `bvx` prefix is equivalent for the observed cases.

Payload contents seen: LZFSE-wrapped SVG (layouts 9/1017), empty stubs (layouts 1019/1020), an uncompressed `bplist00` NSKeyedArchiver archive (layout 1000), and raw `.iconstack` data. scar sniffs magic bytes to give extracted files useful extensions.

### 6.3 MSIS multisize set (magic "SISM", layout 1010)

> **Summary:** A tiny table listing the point sizes an icon is available in.

```
u32 magic "SISM", u32 version(1, assumed), u32 count,
count * { u32 width, u32 height, u32 index }    // e.g. 20/29/40/60pt, index 1..n
```

### 6.4 INLK internal link (TLV 0x3F2, layout 1003, payloadLength 0)

> **Summary:** Not a payload at all — a TLV that says "my pixels are this rectangle of that other rendition (a packed atlas)."

```
u32 magic "KLNI", u32 flags(0), u32 x, u32 y, u32 width, u32 height,
u16 layout (12 = content layout),
u32 keyLength (bytes, unaligned),
keyLength bytes: (u16 attributeId, u16 value) pairs, zero-attr terminated
```

Resolve the target by starting from an all-zero key and applying the pairs in keyformat order; that key names the atlas rendition (element 9 = packed-asset). The crop rect is (x, y, w, h) in the decoded atlas, and **y is bottom-up** (CoreGraphics origin, relative to the atlas height): the top-down pixel row is `atlasHeight − y − h`. Beware: in a single-row atlas the flip is the identity (`H−y−h == y`), so top-down code appears correct until it meets a multi-row atlas.

### 6.5 COLR color (magic "RLOC", layout 1009)

> **Summary:** A color is a colorspace id plus float components. A second trailing RLOC block, when present, names a _system_ color — and dropping it crashes Apple's tools, so it must be preserved.

```
u32 magic "RLOC", u32 version(1), u32 colorSpaceId, u32 nComponents,
nComponents * f64 components
```

- colorSpaceId observed: 0/0x101 (with a system-name block), 1 (sRGB, RGBA), 2 (gray+alpha), 3 (extended/other).
- Components are straight (non-premultiplied) values in [0,1], matching `assetutil`'s readout exactly. Many are f32 values widened to f64 — preserve the exact bits (a correctly-rounded float↔decimal round trip is required; with serde_json that means the `float_roundtrip` feature, or colors drift by 1 ULP).
- **System-color reference:** a second RLOC block may follow immediately: `u32 "RLOC", u32 version(1), u32 nameLen, name (not NUL-terminated)` — e.g. an accent color pointing at "linkColor". Dropping it makes CoreUI/assetutil crash with `object cannot be nil (key: System Color Name)`.

### 6.6 ARGG linear gradient (layout 1021)

> **Summary:** A linear gradient described by axis points and color stops, where each stop references a sibling COLR rendition _by name_.

Decoded and re-encoded byte-exact by `src/argg.rs`; the full byte layout and the disassembly-derived writeup live in that module's doc comment. Icon-group renditions (layout 1020) reference ARGG gradients through TLV 0x3FC by the same name-lookup mechanism.

## 7. Wide / deep pixel formats

> **Summary:** Two "deep" formats store more than 8 bits per channel. They are pixel formats, not compressions — the payload decompresses normally and the deep layout is then converted for display.

**WBGR** (bytes `"WBGR"`, "gamut1" wide-gamut) — **RGBA16F, 8 bytes/pixel**: four little-endian IEEE-754 half floats in **R, G, B, A** order. `bytesPerRow = round32(width*8)`. Values are premultiplied and already in device/display-RGB encoding (gamma applied), so 8-bit conversion is just `clamp(v, 0, 1) * 255` — no ICC matrix. Genuine extended-range values (>1.0) occur and clamp. Editing a WBGR rendition re-encodes by promoting the 8-bit edit back into the RGBA16F container (`src/widegamut.rs`).

**GA16** (bytes `"61AG"`, 'GA16') — **4 bytes/pixel**: two little-endian 16-bit unorm channels, gray then alpha, premultiplied. Convert by taking each channel's high byte, un-premultiplying, and replicating gray to R=G=B. Every GA16 rendition observed in practice is a pure-alpha mask (gray = 0).

Both formats also occur _inside_ deepmap2 (16-bit codec-2 "default" blobs); that path is decoded too — see [deepmap2.md](deepmap2.md) §6. One caveat: saturated Display-P3 photo content gets an extra CoreUI wide-gamut color-management step scar omits (mean ~3/channel drift); in-gamut content is exact.

## 8. Facet, appearance, and localization trees

> **Summary:** Side tables that give renditions human-visible names and map appearance/localization names to the small integers used inside rendition keys.

**FACETKEYS** — key = facet (asset) name string, no NUL. Value:

```
u16 hotSpotX, u16 hotSpotY, u16 nPairs, nPairs * { u16 attributeId, u16 value }
```

A facet groups all renditions whose key matches its element/part/identifier pairs.

**APPEARANCEKEYS** — key = appearance name ("UIAppearanceAny", "UIAppearanceDark", …), value = u16 appearance id (the value renditions carry in their `appearance` key attribute).

**LOCALIZATIONKEYS** — same shape as APPEARANCEKEYS: localization name → u16 id. Present in ~1% of catalogs; must be preserved or localized variants lose their names.

## 9. Compression notes

> **Summary:** LZFSE everywhere, with one trap: LZFSE block magics also occur inside stream bodies, so never scan for magics to find boundaries.

- LZFSE streams are magic-framed blocks: `bvx2` (compressed v2), `bvxn` (v1), `bvx-` (uncompressed), terminated by `bvx$`. The pure-Rust `lzfse_rust` crate handles complete streams both directions.
- `bvx2`/`bvxn` byte sequences occur _inside_ stream bodies too (multi-block streams). Always use explicit lengths (KCBC `compressedLength`, dmp2 tile lengths) to find boundaries — never scan for magics.
