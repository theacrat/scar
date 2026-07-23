//! deepmap2 (CoreUI compression type 11) decode. Full format: docs/deepmap2.md.
//! Output is validated against CoreUI's own rendering (the cuidump oracle).
//!
//! dmp2 16-byte header:
//!
//! ```text
//! [0:4]   "dmp2"
//! [4]     codec method  1=None 2=Default 3=Lossless 4=Palette
//! [5]     blobVersion   (== 1)
//! [6]     innerEncoding (always 0x0a, ignored — use the CSI pixelFormat)
//! [7]     pixelFormat   2=GA8(2Bpp) 4=BGRA(4Bpp) 18=GA16(4Bpp) 20=RGBW(8Bpp)
//! [8:10]  u16 tileW
//! [10:12] u16 tileH
//! [12:16] u32 tile-0 compressed length (doubles as tile 0's length prefix)
//! [16:..] tile streams: LZFSE per tile; tiles 1.. carry their own u32 length
//!         prefix.
//! ```
//!
//! Unsupported variants return Ok(None); the caller passes the payload through
//! verbatim.

use anyhow::Result;

use crate::codec::{self, Pixels};
use crate::format::pixel_format;

const MLEC_HEADER_LEN: usize = 16;
const KCBC_HEADER_LEN: usize = 20;
const WRAPPER_LEN: usize = 16; // { u32 version, u32 encoding, u64 dmpLen }
const DMP2_HEADER_LEN: usize = 16;

const CODEC_NONE: u8 = 1;
const CODEC_DEFAULT: u8 = 2;
const CODEC_LOSSLESS: u8 = 3;
const CODEC_PALETTE: u8 = 4;

/// Decode a full CELM `deepmap2` payload (from the "MLEC" magic) into straight
/// RGBA8; dims/format come from the CSI header. Ok(None) = unsupported variant.
pub fn decode(
    payload: &[u8],
    width: u32,
    height: u32,
    pixel_format: u32,
) -> Result<Option<Pixels>> {
    if payload.len() < MLEC_HEADER_LEN || &payload[0..4] != crate::format::magic::CELM {
        return Ok(None);
    }
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let flags = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let field3 = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as usize;

    // Non-chunked = one whole-image band; CELM flag bit 0 = KCBC row-strip bands.
    let bands = match collect_bands(payload, flags, field3, height) {
        Some(b) => b,
        None => return Ok(None),
    };

    if first_band_is_palette(payload, &bands) {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for band in &bands {
            let wrapper = match payload.get(band.wrapper_off..) {
                Some(w) => w,
                None => return Ok(None),
            };
            let blob = &wrapper[WRAPPER_LEN..];
            let cblk = u32::from_le_bytes(blob[12..16].try_into().unwrap());
            if !decode_palette_band(
                &blob[DMP2_HEADER_LEN..],
                cblk,
                width,
                band.y0,
                band.rows,
                pixel_format,
                &mut rgba,
            )? {
                return Ok(None);
            }
        }
        return Ok(Some(Pixels {
            width,
            height,
            rgba,
        }));
    }

    // Wide Default = 16-bit planar data; wide None/Lossless (RGBA16F) instead
    // takes the generic packed path + `widegamut::to_rgba` below.
    let wide = crate::widegamut::is_wide_format(pixel_format);
    if wide && first_band_codec(payload, &bands) == Some(CODEC_DEFAULT) {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for band in &bands {
            let wrapper = match payload.get(band.wrapper_off..) {
                Some(w) => w,
                None => return Ok(None),
            };
            if !decode_wide_default_band(
                wrapper,
                width,
                band.y0,
                band.rows,
                pixel_format,
                &mut rgba,
            )? {
                return Ok(None);
            }
        }
        return Ok(Some(Pixels {
            width,
            height,
            rgba,
        }));
    }

    // Bytes-per-pixel of the packed premultiplied intermediate.
    let bpp = match pixel_format {
        x if x == pixel_format::ARGB => 4usize,
        x if x == pixel_format::GA8 => 2usize,
        x if x == crate::widegamut::WBGR => 8usize,
        x if x == crate::widegamut::GA16 => 4usize,
        _ => return Ok(None),
    };

    let mut packed = vec![0u8; width as usize * height as usize * bpp];
    for band in &bands {
        let wrapper = match payload.get(band.wrapper_off..) {
            Some(w) => w,
            None => return Ok(None),
        };
        if !decode_band_packed(
            wrapper,
            width,
            band.y0,
            band.rows,
            pixel_format,
            bpp,
            &mut packed,
        )? {
            return Ok(None);
        }
    }

    let bpr = width * bpp as u32;
    if wide {
        return crate::widegamut::to_rgba(&packed, width, height, bpr, pixel_format);
    }
    Ok(Some(codec::raw_to_rgba(
        &packed,
        width,
        height,
        bpr,
        pixel_format,
    )?))
}

/// The dmp2 codec byte of the first band, or None when it can't be read.
fn first_band_codec(payload: &[u8], bands: &[Band]) -> Option<u8> {
    let b = bands.first()?;
    let wrapper = payload.get(b.wrapper_off..)?;
    if wrapper.len() < WRAPPER_LEN + DMP2_HEADER_LEN
        || &wrapper[WRAPPER_LEN..WRAPPER_LEN + 4] != b"dmp2"
    {
        return None;
    }
    Some(wrapper[WRAPPER_LEN + 4])
}

fn first_band_is_palette(payload: &[u8], bands: &[Band]) -> bool {
    let Some(b) = bands.first() else { return false };
    let Some(wrapper) = payload.get(b.wrapper_off..) else {
        return false;
    };
    wrapper.len() >= WRAPPER_LEN + DMP2_HEADER_LEN
        && &wrapper[WRAPPER_LEN..WRAPPER_LEN + 4] == b"dmp2"
        && wrapper[WRAPPER_LEN + 4] == CODEC_PALETTE
}

struct Band {
    /// Offset (within the CELM payload) of the 16-byte dmp2 wrapper.
    wrapper_off: usize,
    /// First image row this band covers.
    y0: u32,
    /// Number of image rows in this band.
    rows: u32,
}

fn collect_bands(payload: &[u8], flags: u32, field3: usize, height: u32) -> Option<Vec<Band>> {
    if flags & 1 == 0 {
        return Some(vec![Band {
            wrapper_off: MLEC_HEADER_LEN,
            y0: 0,
            rows: height,
        }]);
    }
    // `field3` KCBC chunks; KCBC header [12:16] = band row count, [16:20] = chunk byte length.
    let mut bands = Vec::with_capacity(field3);
    let mut off = MLEC_HEADER_LEN;
    let mut y0 = 0u32;
    for _ in 0..field3 {
        let hdr = payload.get(off..off + KCBC_HEADER_LEN)?;
        if &hdr[0..4] != crate::format::magic::KCBC {
            return None;
        }
        let rows = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
        let clen = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
        let wrapper_off = off + KCBC_HEADER_LEN;
        payload.get(wrapper_off..wrapper_off + clen)?;
        bands.push(Band {
            wrapper_off,
            y0,
            rows,
        });
        y0 = y0.checked_add(rows)?;
        off = wrapper_off + clen;
    }
    if y0 != height {
        return None;
    }
    Some(bands)
}

/// Decode one band (dmp2 wrapper+blob, image rows [y0, y0+rows)) into the
/// full-image `packed` premultiplied buffer. Ok(false) = unsupported band.
fn decode_band_packed(
    wrapper: &[u8],
    img_w: u32,
    y0: u32,
    rows: u32,
    pixel_format: u32,
    bpp: usize,
    packed: &mut [u8],
) -> Result<bool> {
    if wrapper.len() < WRAPPER_LEN + DMP2_HEADER_LEN {
        return Ok(false);
    }
    let blob = &wrapper[WRAPPER_LEN..];
    if &blob[0..4] != b"dmp2" {
        return Ok(false);
    }
    let codec = blob[4];
    let tile_w = u16::from_le_bytes(blob[8..10].try_into().unwrap()) as u32;
    let tile_h = u16::from_le_bytes(blob[10..12].try_into().unwrap()) as u32;
    if tile_w == 0 || tile_h == 0 {
        return Ok(false);
    }
    if !matches!(codec, CODEC_NONE | CODEC_DEFAULT | CODEC_LOSSLESS) {
        return Ok(false);
    }

    // Tile stream begins at the u32 tile-0 length prefix (blob[12:16]).
    let mut p = DMP2_HEADER_LEN - 4;
    let mut ty0 = 0u32;
    while ty0 < rows {
        let th = tile_h.min(rows - ty0);
        let mut tx0 = 0u32;
        while tx0 < img_w {
            let tw = tile_w.min(img_w - tx0);
            let Some(len_bytes) = blob.get(p..p + 4) else {
                return Ok(false);
            };
            let tile_len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            p += 4;
            let Some(tile_data) = blob.get(p..p + tile_len) else {
                return Ok(false);
            };
            p += tile_len;

            let tile = match decode_tile_packed(codec, pixel_format, bpp, tile_data, tw, th)? {
                Some(v) => v,
                None => return Ok(false),
            };
            blit_packed(&tile, packed, img_w, bpp, tx0, y0 + ty0, tw, th);
            tx0 += tile_w;
        }
        ty0 += tile_h;
    }
    Ok(true)
}

/// Decode one tile to packed premultiplied pixels (`tw*th*bpp`, unpadded rows).
fn decode_tile_packed(
    codec: u8,
    pixel_format: u32,
    bpp: usize,
    tile_data: &[u8],
    tw: u32,
    th: u32,
) -> Result<Option<Vec<u8>>> {
    let expect = tw as usize * th as usize * bpp;
    match codec {
        CODEC_NONE => {
            // "None" = uncompressed packed pixels; some encoders still LZFSE-wrap.
            let bytes = if tile_data.len() >= 3 && &tile_data[0..3] == b"bvx" {
                match codec::lzfse_decode(tile_data) {
                    Ok(v) => v,
                    Err(_) => return Ok(None),
                }
            } else {
                tile_data.to_vec()
            };
            if bytes.len() != expect {
                return Ok(None);
            }
            Ok(Some(bytes))
        }
        CODEC_LOSSLESS => {
            let bytes = match codec::lzfse_decode(tile_data) {
                Ok(v) => v,
                Err(_) => return Ok(None),
            };
            if bytes.len() != expect {
                return Ok(None);
            }
            Ok(Some(bytes))
        }
        CODEC_DEFAULT => decode_default_tile(pixel_format, tile_data, tw, th),
        _ => Ok(None),
    }
}

/// Default (2) codec for GA8/BGRA: planar `[alpha][selectors][hi][lo]` layout,
/// per-row predictors, YCoCg-R colour inverse. See docs/deepmap2.md §4.
fn decode_default_tile(
    pixel_format: u32,
    tile_data: &[u8],
    tw: u32,
    th: u32,
) -> Result<Option<Vec<u8>>> {
    let (ch, out_bpp) = match pixel_format {
        x if x == pixel_format::GA8 => (1usize, 2usize),
        x if x == pixel_format::ARGB => (3usize, 4usize),
        _ => return Ok(None),
    };
    let wh = tw as usize * th as usize;
    let (val, alpha) = match reconstruct_default_tile(ch, tile_data, tw, th)? {
        Some(v) => v,
        None => return Ok(None),
    };

    let mut packed = vec![0u8; wh * out_bpp];
    if ch == 1 {
        for i in 0..wh {
            packed[i * 2] = val[i].clamp(0, 255) as u8;
            packed[i * 2 + 1] = alpha[i];
        }
    } else {
        for p in 0..wh {
            let (b, g, r) = ycocg_r_to_bgra(val[p * 3], val[p * 3 + 1], val[p * 3 + 2]);
            let o = p * 4;
            // raw_to_rgba (ARGB) reads packed as (b, g, r, a); premultiplied.
            packed[o] = b;
            packed[o + 1] = g;
            packed[o + 2] = r;
            packed[o + 3] = alpha[p];
        }
    }
    Ok(Some(packed))
}

/// Inflate one Default tile into the interleaved (stride `ch`) premultiplied
/// value planes plus the raw alpha plane; shared by the 8-bit and wide paths.
fn reconstruct_default_tile(
    ch: usize,
    tile_data: &[u8],
    tw: u32,
    th: u32,
) -> Result<Option<(Vec<i32>, Vec<u8>)>> {
    let w = tw as usize;
    let h = th as usize;
    let wh = w * h;
    let raw = match codec::lzfse_decode(tile_data) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    // Buffer is align16-padded; only this prefix is read.
    let need = wh + h + 2 * ch * wh;
    if raw.len() < need {
        return Ok(None);
    }
    let alpha = raw[0..wh].to_vec();
    let sel = &raw[wh..wh + h];
    let hi = &raw[wh + h..wh + h + ch * wh];
    let lo = &raw[wh + h + ch * wh..wh + h + 2 * ch * wh];

    let cw = ch * w; // elements per row
    let mut val = vec![0i32; ch * wh];
    #[allow(clippy::needless_range_loop)]
    for y in 0..h {
        let s = sel[y];
        let row = y * cw;
        for x in 0..w {
            let i = row + x * ch;
            // Paeth decides once per pixel from channel 0, then applies to all.
            let use_left = s == 1 && x > 0 && {
                let up0 = if y > 0 { val[i - cw] } else { 0 };
                let ul0 = if y > 0 { val[i - cw - ch] } else { 0 };
                let left0 = val[i - ch];
                (up0 - ul0).abs() <= (left0 - ul0).abs()
            };
            for c in 0..ch {
                let idx = i + c;
                let res16 = ((hi[idx] as i32) << 8) | (lo[idx] as i32);
                let delta = if res16 & 1 != 0 {
                    -(res16 >> 1)
                } else {
                    res16 >> 1
                };
                let left = if x > 0 { val[idx - ch] } else { 0 };
                let up = if y > 0 { val[idx - cw] } else { 0 };
                let pred = match s {
                    2 => left,
                    3 => up,
                    4 => {
                        if x > 0 {
                            (left + up + 1) >> 1
                        } else {
                            up
                        }
                    }
                    1 => {
                        if x == 0 {
                            up
                        } else if use_left {
                            left
                        } else {
                            up
                        }
                    }
                    _ => 0, // None
                };
                val[idx] = delta + pred;
            }
        }
    }
    Ok(Some((val, alpha)))
}

/// Decode one wide Default band (WBGR ch=3, GA16 ch=1) directly into the
/// full-image straight-RGBA8 buffer. Ok(false) for any non-Default band.
fn decode_wide_default_band(
    wrapper: &[u8],
    img_w: u32,
    y0: u32,
    rows: u32,
    pixel_format: u32,
    rgba: &mut [u8],
) -> Result<bool> {
    if wrapper.len() < WRAPPER_LEN + DMP2_HEADER_LEN {
        return Ok(false);
    }
    let blob = &wrapper[WRAPPER_LEN..];
    if &blob[0..4] != b"dmp2" || blob[4] != CODEC_DEFAULT {
        return Ok(false);
    }
    let ch = if pixel_format == crate::widegamut::WBGR {
        3
    } else {
        1
    };
    let tile_w = u16::from_le_bytes(blob[8..10].try_into().unwrap()) as u32;
    let tile_h = u16::from_le_bytes(blob[10..12].try_into().unwrap()) as u32;
    if tile_w == 0 || tile_h == 0 {
        return Ok(false);
    }

    let mut p = DMP2_HEADER_LEN - 4; // index into `blob`, at the tile-0 length u32
    let mut ty0 = 0u32;
    while ty0 < rows {
        let th = tile_h.min(rows - ty0);
        let mut tx0 = 0u32;
        while tx0 < img_w {
            let tw = tile_w.min(img_w - tx0);
            let Some(len_bytes) = blob.get(p..p + 4) else {
                return Ok(false);
            };
            let tile_len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            p += 4;
            let Some(tile_data) = blob.get(p..p + tile_len) else {
                return Ok(false);
            };
            p += tile_len;

            let (val, alpha) = match reconstruct_default_tile(ch, tile_data, tw, th)? {
                Some(v) => v,
                None => return Ok(false),
            };
            let tile = wide_default_tile(pixel_format, &val, &alpha, tw, th);
            blit_packed(&tile, rgba, img_w, 4, tx0, y0 + ty0, tw, th);
            tx0 += tile_w;
        }
        ty0 += tile_h;
    }
    Ok(true)
}

/// Convert a reconstructed wide Default tile to straight RGBA8: wide values
/// span ~2× the 8-bit range, so the premultiplied device byte is `value >> 1`
/// (docs/deepmap2.md §6). Alpha is the raw 8-bit plane.
fn wide_default_tile(pixel_format: u32, val: &[i32], alpha: &[u8], tw: u32, th: u32) -> Vec<u8> {
    let wh = tw as usize * th as usize;
    let half = |x: i32| if x >= 0 { x >> 1 } else { -((-x) >> 1) };
    let q = |v: i32| (v >> 1).clamp(0, 255) as u8; // 2×-range premult value -> device8
    let mut out = vec![0u8; wh * 4];
    for p in 0..wh {
        let a = alpha[p];
        let (pr, pg, pb) = if pixel_format == crate::widegamut::WBGR {
            let (y, c1, c2) = (val[p * 3], val[p * 3 + 1], val[p * 3 + 2]);
            let co = c1 << 1;
            let cg = c2 << 1;
            let t = y - half(cg);
            let g = t + cg;
            let b = t - half(co);
            let r = b + co;
            (q(r), q(g), q(b)) // display R,G,B (RGBA order — no BGRA swap)
        } else {
            let g = q(val[p]);
            (g, g, g)
        };
        let o = p * 4;
        out[o] = unpremultiply(pr, a);
        out[o + 1] = unpremultiply(pg, a);
        out[o + 2] = unpremultiply(pb, a);
        out[o + 3] = a;
    }
    out
}

/// Un-premultiply by 8-bit alpha (round-nearest, saturating).
fn unpremultiply(c: u8, a: u8) -> u8 {
    if a == 0 {
        0
    } else {
        ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
    }
}

/// deepmap2 "YCC" inverse — reversible YCoCg-R lifting (docs/deepmap2.md §4.3).
/// Returns premultiplied bytes in packed BGRA order (display B, G, R).
fn ycocg_r_to_bgra(y: i32, c1: i32, c2: i32) -> (u8, u8, u8) {
    let half = |x: i32| if x >= 0 { x >> 1 } else { -((-x) >> 1) };
    let co = c1 << 1;
    let cg = c2 << 1;
    let t = y - half(cg);
    let g = t + cg;
    let b = t - half(co);
    let r = b + co;
    (
        r.clamp(0, 255) as u8, // packed[0] -> display B
        g.clamp(0, 255) as u8, // packed[1] -> display G
        b.clamp(0, 255) as u8, // packed[2] -> display R
    )
}

/// Blit a decoded tile's packed rows into the full-image buffer at (`x0`, `y0`).
#[allow(clippy::too_many_arguments)]
fn blit_packed(
    tile: &[u8],
    packed: &mut [u8],
    img_w: u32,
    bpp: usize,
    x0: u32,
    y0: u32,
    tw: u32,
    th: u32,
) {
    let iw = img_w as usize;
    let tw = tw as usize;
    let th = th as usize;
    let row_bytes = tw * bpp;
    for ty in 0..th {
        let dst_off = ((y0 as usize + ty) * iw + x0 as usize) * bpp;
        let src = &tile[ty * row_bytes..ty * row_bytes + row_bytes];
        packed[dst_off..dst_off + row_bytes].copy_from_slice(src);
    }
}

/// Palette (4): `[paletteCount*4 straight-BGRA bytes][LZFSE stream of w*h u8
/// indices]`; `cblk = (entrySizeBytes << 16) | (paletteCount - 1)`.
fn decode_palette_band(
    data: &[u8],
    cblk: u32,
    img_w: u32,
    y0: u32,
    rows: u32,
    pixel_format: u32,
    rgba: &mut [u8],
) -> Result<bool> {
    let entry_size = (cblk >> 16) as usize;
    if entry_size != 4 || pixel_format != pixel_format::ARGB {
        return Ok(false);
    }
    let palette_count = ((cblk & 0xFFFF) as usize) + 1;
    let palette_bytes = palette_count * entry_size;
    let Some(palette) = data.get(..palette_bytes) else {
        return Ok(false);
    };
    let index_stream = &data[palette_bytes..];
    let indices = match codec::lzfse_decode(index_stream) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let n = img_w as usize * rows as usize;
    if indices.len() != n {
        return Ok(false);
    }
    let iw = img_w as usize;
    for (k, &ix) in indices.iter().enumerate() {
        let e = ix as usize * 4;
        let (b, g, r, a) = (palette[e], palette[e + 1], palette[e + 2], palette[e + 3]);
        let row = y0 as usize + k / iw;
        let col = k % iw;
        let out = &mut rgba[(row * iw + col) * 4..(row * iw + col) * 4 + 4];
        out[0] = r;
        out[1] = g;
        out[2] = b;
        out[3] = a;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_fixtures")
    }

    fn csi_payload(blob: &[u8]) -> (u32, u32, u32, Vec<u8>) {
        let csi = crate::csi::Csi::parse(blob).unwrap();
        (
            csi.header.width,
            csi.header.height,
            csi.header.pixel_format,
            csi.payload.clone(),
        )
    }

    fn premultiply(c: u8, a: u8) -> u8 {
        ((c as u32 * a as u32 + 127) / 255) as u8
    }

    /// Read a cuidump `.rgbaref`: "RGBA" magic, u32 w, u32 h, PREMULTIPLIED RGBA8 rows.
    fn read_rgbaref(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
        let mut d = fs::read(path).ok()?;
        // Committed refs are LZFSE-wrapped.
        if d.len() >= 3 && &d[0..3] == b"bvx" {
            d = crate::codec::lzfse_decode(&d).ok()?;
        }
        if d.len() < 12 || &d[0..4] != b"RGBA" {
            return None;
        }
        let w = u32::from_le_bytes(d[4..8].try_into().unwrap());
        let h = u32::from_le_bytes(d[8..12].try_into().unwrap());
        Some((w, h, d[12..].to_vec()))
    }

    /// Count channel samples outside ±`tol` vs a premultiplied reference.
    fn compare_premul(px: &Pixels, refw: u32, refh: u32, refpx: &[u8], tol: i32) -> usize {
        if px.width != refw || px.height != refh || refpx.len() != px.rgba.len() {
            return usize::MAX;
        }
        let mut bad = 0;
        for i in 0..(px.width as usize * px.height as usize) {
            let a = px.rgba[i * 4 + 3];
            let want_a = refpx[i * 4 + 3];
            if (a as i32 - want_a as i32).abs() > tol {
                bad += 1;
                continue;
            }
            for c in 0..3 {
                let got = premultiply(px.rgba[i * 4 + c], a);
                let want = refpx[i * 4 + c];
                if (got as i32 - want as i32).abs() > tol {
                    bad += 1;
                    break;
                }
            }
        }
        bad
    }

    /// Wide Default renditions (`wbgr_dmp2_*`, `ga16_*`), re-premultiplied, must
    /// match the CoreUI oracle within ±2/channel (un-premultiply round trip and
    /// extended-range clamps each cost up to ±1).
    #[test]
    fn deepmap2_wide_default_match_oracle() {
        const TOL: i32 = 2;
        let dir = fixtures_dir();
        if !dir.is_dir() {
            eprintln!("no re_fixtures dir, skipping");
            return;
        }
        let mut checked = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let is_wide = name.starts_with("wbgr_dmp2") || name.starts_with("ga16_");
            if !is_wide || !name.ends_with(".csi") {
                continue;
            }
            let Some((rw, rh, oracle)) = read_rgbaref(&dir.join(format!("{name}.rgbaref"))) else {
                continue;
            };
            let blob = fs::read(&path).unwrap();
            let (w, h, pf, payload) = csi_payload(&blob);
            assert!(
                crate::widegamut::is_wide_format(pf),
                "{name}: not a wide pixel format"
            );
            let px = decode(&payload, w, h, pf)
                .unwrap_or_else(|e| panic!("{name}: decode err {e}"))
                .unwrap_or_else(|| panic!("{name}: wide Default decode returned None"));
            assert_eq!((px.width, px.height), (rw, rh), "{name}: dims");
            let mut maxd = 0i32;
            let mut over1 = 0usize;
            for i in 0..(rw * rh) as usize {
                let a = px.rgba[i * 4 + 3];
                let ours = [
                    premultiply(px.rgba[i * 4], a),
                    premultiply(px.rgba[i * 4 + 1], a),
                    premultiply(px.rgba[i * 4 + 2], a),
                    a,
                ];
                for c in 0..4 {
                    let d = (ours[c] as i32 - oracle[i * 4 + c] as i32).abs();
                    maxd = maxd.max(d);
                    if d > 1 {
                        over1 += 1;
                    }
                }
            }
            // Overwhelmingly ≤1; only a handful of saturated highlights hit 2.
            let budget = (rw as usize * rh as usize) / 500 + 8;
            assert!(
                maxd <= TOL,
                "{name}: max per-channel delta {maxd} exceeds {TOL}"
            );
            assert!(
                over1 <= budget,
                "{name}: {over1} channels off by >1 (budget {budget})"
            );
            eprintln!("{name}: {rw}x{rh} max delta {maxd}, {over1} channels off by >1");
            checked += 1;
        }
        assert!(
            checked >= 1,
            "expected at least one wide-Default fixture+oracle"
        );
    }

    /// Every `dmp2*.csi` fixture with a sibling `.rgbaref` must match within ±1.
    #[test]
    fn deepmap2_fixtures_match_cuidump_reference() {
        let dir = fixtures_dir();
        if !dir.is_dir() {
            eprintln!("no re_fixtures dir, skipping");
            return;
        }
        let mut checked = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // Other fixtures in this shared directory are not deepmap2.
            if !name.starts_with("dmp2") || !name.ends_with(".csi") {
                continue;
            }
            let refpath = dir.join(format!("{name}.rgbaref"));
            let Some((rw, rh, rpx)) = read_rgbaref(&refpath) else {
                continue;
            };
            let blob = fs::read(&path).unwrap();
            let (w, h, pf, payload) = csi_payload(&blob);
            let px = decode(&payload, w, h, pf)
                .unwrap_or_else(|e| panic!("{name}: decode err {e}"))
                .unwrap_or_else(|| panic!("{name}: decode returned None but a reference exists"));
            let bad = compare_premul(&px, rw, rh, &rpx, 1);
            // Allow rare rounding disagreements with CoreGraphics' rasterisation.
            let budget = (w as usize * h as usize) / 5000 + 4;
            assert!(
                bad <= budget,
                "{name}: {bad} channels exceed ±1 vs cuidump reference (budget {budget})"
            );
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no .csi fixtures with .rgbaref references found");
        }
    }

    /// Decode every deepmap2 rendition in the RE catalogs; count full matches
    /// (±1) against any same-dimension cuidump oracle in tests/re_refs.
    #[test]
    fn deepmap2_matches_oracle_across_catalogs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cat_dir = root.join("tests/re_catalogs");
        let ref_dir = root.join("tests/re_refs");
        if !cat_dir.is_dir() || !ref_dir.is_dir() {
            eprintln!("no re_catalogs/re_refs, skipping broad validation");
            return;
        }
        let mut oracles: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        for e in fs::read_dir(&ref_dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("rgbaref") {
                continue;
            }
            if let Some((w, h, px)) = read_rgbaref(&p) {
                oracles.push((w, h, px));
            }
        }

        let mut decoded_ok = 0usize;
        let mut validated = 0usize;
        // Unmatched renditions are mostly composited at render time (no direct
        // oracle); a match needs alpha AND colour, so alpha-only can't inflate it.
        for e in fs::read_dir(&cat_dir).unwrap() {
            let data = fs::read(e.unwrap().path()).unwrap();
            let Ok(bom) = crate::bom::Bom::parse(&data) else {
                continue;
            };
            let Ok(rends) = bom.tree_entries("RENDITIONS") else {
                continue;
            };
            for (_k, val) in &rends {
                let Ok(csi) = crate::csi::Csi::parse(val) else {
                    continue;
                };
                if csi.payload.len() < 12 || &csi.payload[0..4] != crate::format::magic::CELM {
                    continue;
                }
                let comp = u32::from_le_bytes(csi.payload[8..12].try_into().unwrap());
                if comp != 11 {
                    continue;
                }
                let (w, h, pf) = (csi.header.width, csi.header.height, csi.header.pixel_format);
                let Some(px) = decode(&csi.payload, w, h, pf).unwrap() else {
                    continue;
                };
                assert_eq!(px.rgba.len(), w as usize * h as usize * 4);
                decoded_ok += 1;
                let n = (w * h) as usize;
                let budget = n / 2000 + 4;
                for (ow, oh, opx) in &oracles {
                    if *ow != w || *oh != h {
                        continue;
                    }
                    if compare_premul(&px, *ow, *oh, opx, 1) <= budget {
                        validated += 1;
                        break;
                    }
                }
            }
        }
        eprintln!(
            "broad deepmap2: decoded {decoded_ok} renditions, {validated} byte-exact (±1) vs a cuidump oracle"
        );
        assert!(
            validated > 50,
            "expected to validate many renditions, only {validated}"
        );
    }

    /// Legacy straight-RGBA references (tools/dmp2_ref) must decode byte-exact.
    #[test]
    fn deepmap2_straight_references_byte_exact() {
        let dir = fixtures_dir();
        if !dir.is_dir() {
            return;
        }
        for prefix in ["dmp2pal_", "dmp2def_"] {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if !name.starts_with(prefix) || !name.ends_with(".csi") {
                    continue;
                }
                let refpath = dir.join(format!("{name}.rgbaref"));
                if !refpath.exists() {
                    continue;
                }
                let mut reference = fs::read(&refpath).unwrap();
                // Committed refs are LZFSE-wrapped.
                if reference.len() >= 3 && &reference[0..3] == b"bvx" {
                    reference = crate::codec::lzfse_decode(&reference).unwrap();
                }
                // Skip cuidump-format refs here (handled by the other test).
                if reference.len() >= 4 && &reference[0..4] == b"RGBA" {
                    continue;
                }
                let blob = fs::read(&path).unwrap();
                let (w, h, pf, payload) = csi_payload(&blob);
                let px = decode(&payload, w, h, pf)
                    .unwrap_or_else(|e| panic!("{name}: {e}"))
                    .unwrap_or_else(|| panic!("{name}: returned None"));
                assert_eq!(px.rgba.len(), reference.len(), "{name}: length mismatch");
                assert_eq!(px.rgba, reference, "{name}: pixels differ from reference");
            }
        }
    }
}
