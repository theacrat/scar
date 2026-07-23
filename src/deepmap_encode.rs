//! deepmap2 (CoreUI compression type 11) encoders; decode lives in `deepmap.rs`.
//! Encoders need not reproduce Apple's bytes — only payloads CoreUI/assetutil
//! accept and decode back to the intended pixels (docs/deepmap2.md §7).
//!
//! Palette (codec 4) framing emitted:
//!
//! ```text
//! "MLEC"  u32 flags(=0)  u32 compressionType(=11)  u32 field3(bytes after hdr)
//!   wrapper:  u32 version(=1)  u32 encoding(=4)  u64 dmpLen(=len of dmp2 blob)
//!   dmp2 blob:
//!      "dmp2" u8 codec(=4) u8 blobVersion(=1) u8 innerEncoding(=0x0a)
//!             u8 pixelFormat(=4 BGRA)  u16 tileW(=width)  u16 tileH(=height)
//!             u32 compressedBlock = ((entrySize=4) << 16) | (paletteCount - 1)
//!      [ paletteCount * 4 : straight BGRA entries ]
//!      [ "bvx2" LZFSE stream : width*height bytes of u8 palette indices ]
//! ```
//!
//! CoreUI hard rules (violations render as garbage even though assetutil exits
//! 0): CELM `flags` must be 0, and `paletteCount = usedColours + 1` — exactly
//! one spare, index-unreachable trailing entry.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::codec::{self, Pixels};
use crate::format::{compression, magic, pixel_format};

const MAX_PALETTE: usize = 256;
const ENTRY_SIZE: u32 = 4; // BGRA

/// Premultiply a straight colour byte by alpha; must match `codec::rgba_to_raw`'s rounding.
fn premultiply(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// deepmap2 Default (codec 2) encoder for BGRA (ch=3) and GA8 (ch=1): single
/// whole-image tile, same MLEC/wrapper framing as palette but codec 2 and
/// `encoding` = pixelFormat byte (docs/deepmap2.md §7.2). Colour reconstructs
/// within ±1, gray exact. Ok(None) for unsupported format or dims > u16.
pub fn encode_default(px: &Pixels, pixel_format: u32) -> Result<Option<Vec<u8>>> {
    let w = px.width as usize;
    let h = px.height as usize;
    if px.rgba.len() != w * h * 4 {
        bail!(
            "encode_default: rgba length {} != {}x{}x4",
            px.rgba.len(),
            w,
            h
        );
    }
    if w == 0 || h == 0 || px.width > u16::MAX as u32 || px.height > u16::MAX as u32 {
        return Ok(None);
    }
    let (ch, fmt_byte) = match pixel_format {
        x if x == pixel_format::ARGB => (3usize, 4u8),
        x if x == pixel_format::GA8 => (1usize, 2u8),
        _ => return Ok(None),
    };
    let wh = w * h;

    // Raw alpha plane + interleaved premultiplied value planes ([Y,C1,C2] or gray).
    let mut alpha = vec![0u8; wh];
    let mut val = vec![0i32; ch * wh];
    for p in 0..wh {
        let s = &px.rgba[p * 4..p * 4 + 4];
        let (r, g, b, a) = (s[0], s[1], s[2], s[3]);
        alpha[p] = a;
        if ch == 1 {
            // GA8 stores the premultiplied gray value (red channel).
            val[p] = premultiply(r, a) as i32;
        } else {
            let pr = premultiply(r, a) as i32;
            let pg = premultiply(g, a) as i32;
            let pb = premultiply(b, a) as i32;
            let (y, c1, c2) = forward_ycocg_r(pr, pg, pb);
            val[p * 3] = y;
            val[p * 3 + 1] = c1;
            val[p * 3 + 2] = c2;
        }
    }

    let cw = ch * w; // interleaved elements per row
    let mut sel = vec![0u8; h];
    let mut hi = vec![0u8; ch * wh];
    let mut lo = vec![0u8; ch * wh];
    #[allow(clippy::needless_range_loop)]
    for y in 0..h {
        // Row 0 has no "up": restrict to None/Left, matching Apple's encoder.
        let cands: &[u8] = if y == 0 {
            &[0, 2]
        } else {
            default_selector_set()
        };
        let mut best_sel = cands[0];
        let mut best_cost = u64::MAX;
        for &s in cands {
            let cost = row_residual_cost(&val, w, ch, cw, y, s);
            if cost < best_cost {
                best_cost = cost;
                best_sel = s;
            }
        }
        sel[y] = best_sel;
        emit_row(&val, &mut hi, &mut lo, w, ch, cw, y, best_sel);
    }

    // Pad to align16, matching Apple's expected raw buffer size.
    let need = wh + h + 2 * ch * wh;
    let padded = (need + 15) & !15;
    let mut planar = Vec::with_capacity(padded);
    planar.extend_from_slice(&alpha);
    planar.extend_from_slice(&sel);
    planar.extend_from_slice(&hi);
    planar.extend_from_slice(&lo);
    planar.resize(padded, 0);

    let tile = codec::lzfse_encode(&planar);

    let mut blob = Vec::with_capacity(16 + tile.len());
    blob.extend_from_slice(b"dmp2");
    blob.push(2); // codec: Default
    blob.push(1); // blobVersion
    blob.push(0x0a); // innerEncoding (ignored by decode)
    blob.push(fmt_byte); // pixelFormat: 4 BGRA / 2 GA8
    blob.extend_from_slice(&(px.width as u16).to_le_bytes()); // tileW = width
    blob.extend_from_slice(&(px.height as u16).to_le_bytes()); // tileH = height
    blob.extend_from_slice(&(tile.len() as u32).to_le_bytes()); // tile-0 length
    blob.extend_from_slice(&tile);

    // Wrapper { u32 version, u32 encoding(=pixelFormat byte), u64 dmpLen }.
    let mut wrapper = Vec::with_capacity(16);
    wrapper.extend_from_slice(&1u32.to_le_bytes());
    wrapper.extend_from_slice(&(fmt_byte as u32).to_le_bytes());
    wrapper.extend_from_slice(&(blob.len() as u64).to_le_bytes());

    // flags MUST be 0 or CUICatalog decodes garbage.
    let field3 = (wrapper.len() + blob.len()) as u32;
    let mut payload = Vec::with_capacity(16 + wrapper.len() + blob.len());
    payload.extend_from_slice(magic::CELM);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&compression::DEEPMAP2.to_le_bytes());
    payload.extend_from_slice(&field3.to_le_bytes());
    payload.extend_from_slice(&wrapper);
    payload.extend_from_slice(&blob);
    Ok(Some(payload))
}

/// Selector candidates for rows > 0 (override: `SCAR_ENC_SELS`). Mean (4) is
/// excluded — Apple truncates `(left+up+1)/2` toward zero, not `>>1` toward −∞,
/// so Mean rows drift under CUICatalog on negative chroma.
fn default_selector_set() -> &'static [u8] {
    use std::sync::OnceLock;
    static SELS: OnceLock<Vec<u8>> = OnceLock::new();
    SELS.get_or_init(|| match std::env::var("SCAR_ENC_SELS") {
        Ok(s) => s.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
        Err(_) => vec![0, 1, 2, 3],
    })
    .as_slice()
}

/// Forward YCoCg-R: premultiplied (R,G,B) → stored (Y,C1,C2), within ±1 under
/// both decoders. Snaps to the nearest in-gamut lattice point (±2 cube) with a
/// truncation-aware cost: Apple stores the low 8 bits unclamped, so a
/// reconstructed −1 renders as 255 (docs/deepmap2.md §7.2).
fn forward_ycocg_r(pr: i32, pg: i32, pb: i32) -> (i32, i32, i32) {
    let mut best: Option<(i32, i32, i32)> = None;
    let mut best_cost = i32::MAX;
    for db in -2..=2 {
        for dg in -2..=2 {
            for dr in -2..=2 {
                let (b2, g2, r2) = (pb + db, pg + dg, pr + dr);
                // Lattice membership: B≡R (mod 2) and B+R−2G ≡ 0 (mod 4).
                if (b2 - r2) & 1 != 0 || (b2 + r2 - 2 * g2).rem_euclid(4) != 0 {
                    continue;
                }
                // rem_euclid(256) charges out-of-gamut channels the full wrap distance.
                let eb = b2.rem_euclid(256) - pb;
                let eg = g2.rem_euclid(256) - pg;
                let er = r2.rem_euclid(256) - pr;
                let cost = eb * eb + eg * eg + er * er;
                if cost < best_cost {
                    best_cost = cost;
                    best = Some((b2, g2, r2));
                }
            }
        }
    }
    let (b2, g2, r2) = best.expect("YCoCg-R lattice always has an in-gamut point within ±2");
    let c1 = (b2 - r2) / 2; // B − R = 2·C1
    let y = (g2 + r2 + c1) / 2; // R + G = 2Y − C1
    let c2 = g2 - y; // G = Y + C2
    (y, c1, c2)
}

/// Predicted value for element `idx` under selector `s`; `use_left` is the
/// per-pixel Paeth decision.
#[allow(clippy::too_many_arguments)]
fn predict(
    val: &[i32],
    ch: usize,
    cw: usize,
    y: usize,
    x: usize,
    idx: usize,
    s: u8,
    use_left: bool,
) -> i32 {
    let left = if x > 0 { val[idx - ch] } else { 0 };
    let up = if y > 0 { val[idx - cw] } else { 0 };
    match s {
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
    }
}

/// Per-pixel Paeth gradient choice, decided from channel 0 (`i0`).
fn paeth_use_left(val: &[i32], ch: usize, cw: usize, y: usize, x: usize, i0: usize) -> bool {
    if x == 0 {
        return false;
    }
    let up0 = if y > 0 { val[i0 - cw] } else { 0 };
    let ul0 = if y > 0 { val[i0 - cw - ch] } else { 0 };
    let left0 = val[i0 - ch];
    (up0 - ul0).abs() <= (left0 - ul0).abs()
}

/// Fold a signed delta into the LSB-sign 16-bit residual the decoder unfolds.
fn fold(delta: i32) -> u16 {
    if delta < 0 {
        (((-delta) as u32) << 1 | 1) as u16
    } else {
        ((delta as u32) << 1) as u16
    }
}

/// Sum of folded residual magnitudes for row `y` under selector `s` (compressibility proxy).
fn row_residual_cost(val: &[i32], w: usize, ch: usize, cw: usize, y: usize, s: u8) -> u64 {
    let row = y * cw;
    let mut cost = 0u64;
    for x in 0..w {
        let i0 = row + x * ch;
        let use_left = s == 1 && paeth_use_left(val, ch, cw, y, x, i0);
        for c in 0..ch {
            let idx = i0 + c;
            let pred = predict(val, ch, cw, y, x, idx, s, use_left);
            cost += fold(val[idx] - pred) as u64;
        }
    }
    cost
}

/// Emit row `y`'s folded residuals into the hi/lo planes under selector `s`.
#[allow(clippy::too_many_arguments)]
fn emit_row(
    val: &[i32],
    hi: &mut [u8],
    lo: &mut [u8],
    w: usize,
    ch: usize,
    cw: usize,
    y: usize,
    s: u8,
) {
    let row = y * cw;
    for x in 0..w {
        let i0 = row + x * ch;
        let use_left = s == 1 && paeth_use_left(val, ch, cw, y, x, i0);
        for c in 0..ch {
            let idx = i0 + c;
            let pred = predict(val, ch, cw, y, x, idx, s, use_left);
            let res16 = fold(val[idx] - pred);
            hi[idx] = (res16 >> 8) as u8;
            lo[idx] = (res16 & 0xff) as u8;
        }
    }
}

/// Encode straight RGBA8 into a deepmap2 palette CELM payload (BGRA rendition).
/// Ok(None) when zero-sized or a dimension exceeds the u16 tile fields.
pub fn encode_palette(px: &Pixels) -> Result<Option<Vec<u8>>> {
    let w = px.width as usize;
    let h = px.height as usize;
    if px.rgba.len() != w * h * 4 {
        bail!(
            "encode_palette: rgba length {} != {}x{}x4",
            px.rgba.len(),
            w,
            h
        );
    }
    if w == 0 || h == 0 || px.width > u16::MAX as u32 || px.height > u16::MAX as u32 {
        return Ok(None);
    }

    let (mut palette_bgra, indices) = build_palette(&px.rgba);
    debug_assert_eq!(indices.len(), w * h);
    let used = palette_bgra.len() / 4;
    debug_assert!((1..=MAX_PALETTE).contains(&used));

    // Mandatory spare unused entry; without it CUICatalog renders the top index transparent.
    palette_bgra.extend_from_slice(&[0, 0, 0, 0]);
    let count = used + 1;

    let cblk: u32 = (ENTRY_SIZE << 16) | ((count - 1) as u32);
    let index_stream = codec::lzfse_encode(&indices);

    let mut blob = Vec::with_capacity(16 + palette_bgra.len() + index_stream.len());
    blob.extend_from_slice(b"dmp2");
    blob.push(4); // codec: palette
    blob.push(1); // blobVersion
    blob.push(0x0a); // innerEncoding (ignored by decode)
    blob.push(4); // pixelFormat: BGRA
    blob.extend_from_slice(&(px.width as u16).to_le_bytes()); // tileW
    blob.extend_from_slice(&(px.height as u16).to_le_bytes()); // tileH
    blob.extend_from_slice(&cblk.to_le_bytes());
    blob.extend_from_slice(&palette_bgra);
    blob.extend_from_slice(&index_stream);

    // Wrapper { u32 version, u32 encoding, u64 dmpLen }.
    let mut wrapper = Vec::with_capacity(16);
    wrapper.extend_from_slice(&1u32.to_le_bytes()); // version
    wrapper.extend_from_slice(&4u32.to_le_bytes()); // encoding (4 = palette slot)
    wrapper.extend_from_slice(&(blob.len() as u64).to_le_bytes());

    // field3 = bytes after the 16-byte MLEC header.
    let field3 = (wrapper.len() + blob.len()) as u32;
    let mut payload = Vec::with_capacity(16 + wrapper.len() + blob.len());
    payload.extend_from_slice(magic::CELM);
    payload.extend_from_slice(&0u32.to_le_bytes()); // flags = 0 (non-chunked; CoreUI-required)
    payload.extend_from_slice(&compression::DEEPMAP2.to_le_bytes());
    payload.extend_from_slice(&field3.to_le_bytes());
    payload.extend_from_slice(&wrapper);
    payload.extend_from_slice(&blob);

    let _ = pixel_format::ARGB; // documents the target format for this payload
    Ok(Some(payload))
}

/// Build straight-BGRA palette bytes + per-pixel u8 indices. ≤256 distinct
/// colours is exact (first-appearance order); otherwise median-cut to 256.
fn build_palette(rgba: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut index_of: HashMap<[u8; 4], u32> = HashMap::new();
    let mut distinct: Vec<([u8; 4], u32)> = Vec::new();
    for px in rgba.chunks_exact(4) {
        let c = [px[0], px[1], px[2], px[3]];
        match index_of.get(&c) {
            Some(&i) => distinct[i as usize].1 += 1,
            None => {
                index_of.insert(c, distinct.len() as u32);
                distinct.push((c, 1));
            }
        }
    }

    let (palette_rgba, color_to_index): (Vec<[u8; 4]>, HashMap<[u8; 4], u8>) =
        if distinct.len() <= MAX_PALETTE {
            let pal: Vec<[u8; 4]> = distinct.iter().map(|&(c, _)| c).collect();
            let map: HashMap<[u8; 4], u8> = distinct
                .iter()
                .enumerate()
                .map(|(i, &(c, _))| (c, i as u8))
                .collect();
            (pal, map)
        } else {
            let pal = median_cut(&mut distinct, MAX_PALETTE);
            let mut map: HashMap<[u8; 4], u8> = HashMap::with_capacity(distinct.len());
            for &(c, _) in &distinct {
                map.insert(c, nearest(&pal, c));
            }
            (pal, map)
        };

    let mut indices = Vec::with_capacity(rgba.len() / 4);
    for px in rgba.chunks_exact(4) {
        let c = [px[0], px[1], px[2], px[3]];
        indices.push(color_to_index[&c]);
    }

    let mut palette_bgra = Vec::with_capacity(palette_rgba.len() * 4);
    for c in &palette_rgba {
        palette_bgra.extend_from_slice(&[c[2], c[1], c[0], c[3]]);
    }
    (palette_bgra, indices)
}

/// Nearest palette entry to `c` by squared Euclidean distance over RGBA.
fn nearest(palette: &[[u8; 4]], c: [u8; 4]) -> u8 {
    let mut best = 0usize;
    let mut best_d = i64::MAX;
    for (i, p) in palette.iter().enumerate() {
        let mut d = 0i64;
        for k in 0..4 {
            let diff = c[k] as i64 - p[k] as i64;
            d += diff * diff;
        }
        if d < best_d {
            best_d = d;
            best = i;
            if d == 0 {
                break;
            }
        }
    }
    best as u8
}

/// Median-cut quantisation of `distinct` (colour, count) pairs to ≤`max` RGBA colours.
fn median_cut(distinct: &mut [([u8; 4], u32)], max: usize) -> Vec<[u8; 4]> {
    // A box is a half-open index range [s, e) into `distinct`.
    let mut boxes: Vec<(usize, usize)> = vec![(0, distinct.len())];
    while boxes.len() < max {
        let mut pick: Option<(usize, usize)> = None; // (box index, channel)
        let mut best_score = 0i64;
        for (bi, &(s, e)) in boxes.iter().enumerate() {
            if e - s < 2 {
                continue;
            }
            let (ch, range) = longest_channel(&distinct[s..e]);
            let pop: u32 = distinct[s..e].iter().map(|x| x.1).sum();
            let score = range as i64 * pop as i64;
            if score > best_score {
                best_score = score;
                pick = Some((bi, ch));
            }
        }
        let Some((bi, ch)) = pick else { break };
        let (s, e) = boxes[bi];
        distinct[s..e].sort_by_key(|x| x.0[ch]);
        // Split at the count-weighted median so each half holds ~half the pixels.
        let total: u32 = distinct[s..e].iter().map(|x| x.1).sum();
        let mut acc = 0u32;
        let mut mid = s + 1;
        for (i, item) in distinct[s..e].iter().enumerate() {
            acc += item.1;
            if acc * 2 >= total {
                mid = s + i + 1;
                break;
            }
        }
        mid = mid.clamp(s + 1, e - 1);
        boxes[bi] = (s, mid);
        boxes.push((mid, e));
    }

    // Representative = count-weighted average of each box.
    boxes
        .iter()
        .map(|&(s, e)| {
            let mut sum = [0u64; 4];
            let mut cnt = 0u64;
            for &(c, w) in &distinct[s..e] {
                let w = w as u64;
                for k in 0..4 {
                    sum[k] += c[k] as u64 * w;
                }
                cnt += w;
            }
            let mut rep = [0u8; 4];
            for k in 0..4 {
                rep[k] = ((sum[k] + cnt / 2) / cnt) as u8;
            }
            rep
        })
        .collect()
}

/// Channel (0..4) with the greatest max−min spread, and that spread.
fn longest_channel(colors: &[([u8; 4], u32)]) -> (usize, u16) {
    let mut min = [255u8; 4];
    let mut max = [0u8; 4];
    for &(c, _) in colors {
        for k in 0..4 {
            min[k] = min[k].min(c[k]);
            max[k] = max[k].max(c[k]);
        }
    }
    let mut best_ch = 0;
    let mut best_range = 0u16;
    for k in 0..4 {
        let r = (max[k] - min[k]) as u16;
        if r > best_range {
            best_range = r;
            best_ch = k;
        }
    }
    (best_ch, best_range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepmap;
    use std::collections::HashSet;

    /// ≤256 distinct colours must round-trip byte-exactly.
    #[test]
    fn palette_round_trip_byte_exact() {
        // 16x16, 256 distinct colours, all four channels varying.
        let w = 16u32;
        let h = 16u32;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            let v = i as u8;
            rgba.extend_from_slice(&[v, v.wrapping_mul(3), v.wrapping_mul(7), v]);
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba,
        };

        let payload = encode_palette(&px).unwrap().expect("should encode");
        assert_eq!(&payload[0..4], magic::CELM);
        let decoded = deepmap::decode(&payload, w, h, pixel_format::ARGB)
            .unwrap()
            .expect("decode should support our palette payload");
        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);
        assert_eq!(
            decoded.rgba, px.rgba,
            "palette round trip must be byte-exact"
        );
    }

    /// A tiny (few-colour) image round-trips and produces the minimal palette.
    #[test]
    fn palette_round_trip_few_colors() {
        let w = 4u32;
        let h = 4u32;
        let colors = [[255u8, 0, 0, 255], [0, 255, 0, 128], [0, 0, 255, 0]];
        let mut rgba = Vec::new();
        for i in 0..(w * h) as usize {
            rgba.extend_from_slice(&colors[i % colors.len()]);
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_palette(&px).unwrap().unwrap();
        let decoded = deepmap::decode(&payload, w, h, pixel_format::ARGB)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.rgba, rgba);
    }

    /// >256 distinct colours: quantisation must stay ≤256 colours, keep the mean
    /// error small, and be a stable fixpoint on re-encode.
    #[test]
    fn palette_quantization_sanity() {
        let w = 64u32;
        let h = 64u32; // 4096 pixels
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]);
            }
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };

        let distinct: HashSet<[u8; 4]> = rgba
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        assert!(distinct.len() > 256, "test image must exceed 256 colours");

        let payload = encode_palette(&px).unwrap().unwrap();
        let decoded = deepmap::decode(&payload, w, h, pixel_format::ARGB)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);

        let dec_distinct: HashSet<[u8; 4]> = decoded
            .rgba
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        assert!(
            dec_distinct.len() <= 256,
            "quantised palette must be ≤256 colours"
        );

        let mut err = 0u64;
        for (a, b) in rgba.iter().zip(decoded.rgba.iter()) {
            err += (*a as i64 - *b as i64).unsigned_abs();
        }
        let mean = err as f64 / rgba.len() as f64;
        assert!(
            mean < 12.0,
            "mean channel error {mean} too high for a gradient"
        );

        let payload2 = encode_palette(&decoded).unwrap().unwrap();
        let decoded2 = deepmap::decode(&payload2, w, h, pixel_format::ARGB)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded2.rgba, decoded.rgba,
            "quantised image must be a stable fixpoint"
        );
    }

    /// Zero-sized images are declined (Ok(None)).
    #[test]
    fn palette_declines_empty() {
        let px = Pixels {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert!(encode_palette(&px).unwrap().is_none());
    }

    fn decode_default(payload: &[u8], w: u32, h: u32, pf: u32) -> Pixels {
        deepmap::decode(payload, w, h, pf)
            .expect("decode err")
            .expect("decode returned None for our own Default payload")
    }

    fn max_channel_diff(a: &[u8], b: &[u8]) -> i32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b.iter())
            .map(|(&x, &y)| (x as i32 - y as i32).abs())
            .max()
            .unwrap_or(0)
    }

    /// GA8 Default is exact (luma stored verbatim); alpha=255 so premult==straight.
    #[test]
    fn default_ga8_round_trip_byte_exact() {
        let (w, h) = (24u32, 17u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // Gradient + a hard vertical edge to exercise every predictor.
                let g = if x < w / 2 {
                    (x * 9) as u8
                } else {
                    255u8.wrapping_sub((y * 7) as u8)
                };
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_default(&px, pixel_format::GA8)
            .unwrap()
            .expect("should encode");
        assert_eq!(&payload[0..4], magic::CELM);
        let dec = decode_default(&payload, w, h, pixel_format::GA8);
        assert_eq!(dec.rgba, rgba, "GA8 Default round trip must be byte-exact");
    }

    /// GA8 with varying alpha: gray and alpha both exact in the premultiplied domain.
    #[test]
    fn default_ga8_alpha_exact() {
        let (w, h) = (8u32, 8u32);
        let mut rgba = Vec::new();
        for i in 0..(w * h) as usize {
            let a = (i * 3) as u8;
            let g = (200 - i) as u8;
            rgba.extend_from_slice(&[g, g, g, a]);
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_default(&px, pixel_format::GA8).unwrap().unwrap();
        let dec = decode_default(&payload, w, h, pixel_format::GA8);
        // Compare premultiplied: unpremultiply at low alpha would drift.
        for i in 0..(w * h) as usize {
            let a = rgba[i * 4 + 3];
            assert_eq!(dec.rgba[i * 4 + 3], a, "alpha exact at {i}");
            if a == 0 {
                continue;
            }
            let want = premultiply(rgba[i * 4], a);
            let got = premultiply(dec.rgba[i * 4], a);
            assert_eq!(got, want, "premult gray exact at {i}");
        }
    }

    /// Grayscale colours are exact lattice points, so alpha=255 gray-in-BGRA
    /// round-trips byte-exact through the colour transform.
    #[test]
    fn default_bgra_gray_lattice_byte_exact() {
        let (w, h) = (20u32, 13u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 11 + y * 5) & 0xff) as u8;
                rgba.extend_from_slice(&[v, v, v, 255]); // gray -> exact
            }
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_default(&px, pixel_format::ARGB).unwrap().unwrap();
        let dec = decode_default(&payload, w, h, pixel_format::ARGB);
        assert_eq!(dec.rgba, rgba, "gray-in-BGRA Default must be byte-exact");
    }

    /// Arbitrary colours (alpha=255) reconstruct within ±1 (8-bit chroma lattice).
    #[test]
    fn default_bgra_color_within_one() {
        let (w, h) = (32u32, 32u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]);
            }
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_default(&px, pixel_format::ARGB).unwrap().unwrap();
        let dec = decode_default(&payload, w, h, pixel_format::ARGB);
        assert!(
            max_channel_diff(&dec.rgba, &rgba) <= 1,
            "colour Default must reconstruct within ±1, got {}",
            max_channel_diff(&dec.rgba, &rgba)
        );
    }

    /// BGRA colour Default with alpha<255: premultiplied domain, still within ±1.
    #[test]
    fn default_bgra_alpha_within_one() {
        let (w, h) = (16u32, 16u32);
        let mut rgba = Vec::new();
        for i in 0..(w * h) as usize {
            let a = (i * 5) as u8;
            rgba.extend_from_slice(&[(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, a]);
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba: rgba.clone(),
        };
        let payload = encode_default(&px, pixel_format::ARGB).unwrap().unwrap();
        let dec = decode_default(&payload, w, h, pixel_format::ARGB);
        // Compare premultiplied: unpremultiply at low alpha amplifies error.
        for i in 0..(w * h) as usize {
            let a = rgba[i * 4 + 3];
            assert_eq!(dec.rgba[i * 4 + 3], a, "alpha exact at {i}");
            if a == 0 {
                continue;
            }
            for c in 0..3 {
                let want = premultiply(rgba[i * 4 + c], a) as i32;
                let got = premultiply(dec.rgba[i * 4 + c], a) as i32;
                assert!(
                    (got - want).abs() <= 1,
                    "premult chan within ±1 at pixel {i} chan {c}"
                );
            }
        }
    }

    /// `forward_ycocg_r` composed with Apple's truncating inverse stays within
    /// ±1 across a dense sample of the RGB cube.
    #[test]
    fn forward_ycocg_r_within_one_dense() {
        // Apple's inverse truncates to 8 bits (rem_euclid 256), no clamp.
        fn inverse(y: i32, c1: i32, c2: i32) -> (i32, i32, i32) {
            let half = |x: i32| if x >= 0 { x >> 1 } else { -((-x) >> 1) };
            let co = c1 << 1;
            let cg = c2 << 1;
            let t = y - half(cg);
            let g = t + cg;
            let b = t - half(co);
            let r = b + co;
            // returned as (lifting_r=display B, lifting_g=G, lifting_b=display R)
            (r.rem_euclid(256), g.rem_euclid(256), b.rem_euclid(256))
        }
        let mut worst = 0;
        let step = 3;
        for r in (0..=255).step_by(step) {
            for g in (0..=255).step_by(step) {
                for b in (0..=255).step_by(step) {
                    let (y, c1, c2) = forward_ycocg_r(r, g, b);
                    // decoder: pB=display Blue, pG, pR=display Red
                    let (pb, pg, pr) = inverse(y, c1, c2);
                    let d = (pb - b).abs().max((pg - g).abs()).max((pr - r).abs());
                    worst = worst.max(d);
                }
            }
        }
        assert!(
            worst <= 1,
            "forward_ycocg_r worst-case reconstruction error {worst} > 1"
        );
    }

    /// Unsupported formats / oversized dims decline gracefully.
    #[test]
    fn default_declines_unsupported() {
        let px = Pixels {
            width: 2,
            height: 2,
            rgba: vec![0u8; 16],
        };
        assert!(encode_default(&px, pixel_format::SVG).unwrap().is_none());
        let empty = Pixels {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert!(
            encode_default(&empty, pixel_format::ARGB)
                .unwrap()
                .is_none()
        );
    }
}
