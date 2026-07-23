//! Wide/deep pixel-format conversion to straight RGBA8 (docs/FORMAT.md §7).
//!
//! WBGR: 8 B/px, four LE half floats in R,G,B,A order despite the name;
//! premultiplied, display-encoded — 8-bit is `clamp(v,0,1)*255`, no ICC matrix.
//! GA16: 4 B/px, two LE u16 unorm channels (gray, alpha), premultiplied.

use anyhow::{Result, bail};

use crate::codec::Pixels;

/// Pixel-format tag "WBGR" (on-disk bytes W,B,G,R) — wide-gamut RGBA16F.
pub const WBGR: u32 = u32::from_le_bytes(*b"WBGR");

/// Pixel-format tag 'GA16' (on-disk bytes 6,1,A,G) — 16-bit gray + alpha.
pub const GA16: u32 = u32::from_le_bytes(*b"61AG");

/// True for the deep/wide pixel formats this module knows how to convert.
pub fn is_wide_format(pixel_format: u32) -> bool {
    pixel_format == WBGR || pixel_format == GA16
}

/// Convert decompressed wide-format rows into straight RGBA8.
/// Ok(None) for formats not handled here (caller passes through).
pub fn to_rgba(
    raw: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
    pixel_format: u32,
) -> Result<Option<Pixels>> {
    match pixel_format {
        WBGR => Ok(Some(wbgr_to_rgba(raw, width, height, bytes_per_row)?)),
        GA16 => Ok(Some(ga16_to_rgba(raw, width, height, bytes_per_row)?)),
        _ => Ok(None),
    }
}

/// f32 -> IEEE-754 binary16, round-to-nearest-even; inverse of `half_to_f32`.
fn f32_to_half(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let unbiased = exp - 127 + 15;
    if unbiased >= 0x1f {
        return sign | 0x7c00;
    }
    if unbiased <= 0 {
        // Subnormal or zero.
        if unbiased < -10 {
            return sign;
        }
        let mant_full = mant | 0x80_0000;
        let shift = (14 - unbiased) as u32;
        let half_mant = mant_full >> shift;
        let round = (mant_full >> (shift - 1)) & 1;
        return sign | ((half_mant as u16) + round as u16);
    }
    let half_exp = (unbiased as u16) << 10;
    let half_mant = (mant >> 13) as u16;
    let round = (mant >> 12) & 1;
    (sign | half_exp | half_mant) + round as u16
}

/// Straight RGBA8 -> premultiplied RGBA16F (WBGR) rows; inverse of `wbgr_to_rgba`.
pub fn rgba_to_wbgr_raw(px: &Pixels, bytes_per_row: u32) -> Vec<u8> {
    let w = px.width as usize;
    let h = px.height as usize;
    let bpr = bytes_per_row as usize;
    const BPP: usize = 8;
    let mut raw = vec![0u8; bpr * h];
    for y in 0..h {
        for x in 0..w {
            let s = &px.rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
            let a = s[3];
            // Channel order is R,G,B,A despite the "WBGR" tag.
            let comps = [
                (premul_u8(s[0], a) as f32) / 255.0,
                (premul_u8(s[1], a) as f32) / 255.0,
                (premul_u8(s[2], a) as f32) / 255.0,
                (a as f32) / 255.0,
            ];
            let off = y * bpr + x * BPP;
            for (i, c) in comps.iter().enumerate() {
                raw[off + i * 2..off + i * 2 + 2].copy_from_slice(&f32_to_half(*c).to_le_bytes());
            }
        }
    }
    raw
}

fn premul_u8(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// IEEE-754 binary16 -> f32; NaN/Inf pass through (clamped later by `unit_to_u8`).
fn half_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let mant = h & 0x3ff;
    let val = if exp == 0 {
        // Subnormal (or zero): mant * 2^-24.
        (mant as f32) * (1.0f32 / 16_777_216.0)
    } else if exp == 0x1f {
        if mant == 0 { f32::INFINITY } else { f32::NAN }
    } else {
        (1.0 + mant as f32 / 1024.0) * 2.0f32.powi(exp as i32 - 15)
    };
    if sign == 1 { -val } else { val }
}

/// Clamp to [0,1] and quantize to 8-bit (round-to-nearest); NaN maps to 0.
fn unit_to_u8(v: f32) -> u8 {
    let c = v.clamp(0.0, 1.0);
    (c * 255.0 + 0.5) as u8
}

/// Un-premultiply an 8-bit channel against an 8-bit alpha.
fn unpremultiply(c: u8, a: u8) -> u8 {
    if a == 0 {
        0
    } else {
        ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
    }
}

/// WBGR (RGBA16F, premultiplied, device-RGB encoded) → straight RGBA8.
fn wbgr_to_rgba(raw: &[u8], width: u32, height: u32, bytes_per_row: u32) -> Result<Pixels> {
    let w = width as usize;
    let h = height as usize;
    let bpr = bytes_per_row as usize;
    const BPP: usize = 8;
    if bpr < w * BPP {
        bail!("wbgr_to_rgba: bytes_per_row {bpr} too small for width {w}");
    }
    if raw.len() < bpr * h {
        bail!(
            "wbgr_to_rgba: raw buffer too short ({} < {})",
            raw.len(),
            bpr * h
        );
    }
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &raw[y * bpr..y * bpr + w * BPP];
        for x in 0..w {
            let px = &row[x * BPP..x * BPP + BPP];
            let r = half_to_f32(u16::from_le_bytes([px[0], px[1]]));
            let g = half_to_f32(u16::from_le_bytes([px[2], px[3]]));
            let b = half_to_f32(u16::from_le_bytes([px[4], px[5]]));
            let a = half_to_f32(u16::from_le_bytes([px[6], px[7]]));
            let a8 = unit_to_u8(a);
            let out = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
            out[0] = unpremultiply(unit_to_u8(r), a8);
            out[1] = unpremultiply(unit_to_u8(g), a8);
            out[2] = unpremultiply(unit_to_u8(b), a8);
            out[3] = a8;
        }
    }
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

/// GA16 (16-bit gray + 16-bit alpha unorm, premultiplied) → straight RGBA8.
fn ga16_to_rgba(raw: &[u8], width: u32, height: u32, bytes_per_row: u32) -> Result<Pixels> {
    let w = width as usize;
    let h = height as usize;
    let bpr = bytes_per_row as usize;
    const BPP: usize = 4;
    if bpr < w * BPP {
        bail!("ga16_to_rgba: bytes_per_row {bpr} too small for width {w}");
    }
    if raw.len() < bpr * h {
        bail!(
            "ga16_to_rgba: raw buffer too short ({} < {})",
            raw.len(),
            bpr * h
        );
    }
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &raw[y * bpr..y * bpr + w * BPP];
        for x in 0..w {
            let px = &row[x * BPP..x * BPP + BPP];
            let gray16 = u16::from_le_bytes([px[0], px[1]]);
            let alpha16 = u16::from_le_bytes([px[2], px[3]]);
            let a8 = (alpha16 >> 8) as u8;
            let gray8 = unpremultiply((gray16 >> 8) as u8, a8);
            let out = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
            out[0] = gray8;
            out[1] = gray8;
            out[2] = gray8;
            out[3] = a8;
        }
    }
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;
    use crate::csi::Csi;
    use crate::format::tlv;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_fixtures")
    }

    #[test]
    fn f32_half_round_trips_over_unit_range() {
        for v in 0u32..=255 {
            let f = v as f32 / 255.0;
            let back = half_to_f32(f32_to_half(f));
            assert_eq!(unit_to_u8(back), v as u8, "value {v} did not round-trip");
        }
    }

    #[test]
    fn rgba_to_wbgr_raw_round_trips_within_one() {
        let w = 8u32;
        let h = 8u32;
        let mut rgba = Vec::new();
        for i in 0..(w * h) {
            let a = if i % 5 == 0 { 128 } else { 255 };
            rgba.extend_from_slice(&[(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, a]);
        }
        let px = Pixels {
            width: w,
            height: h,
            rgba,
        };
        let bpr = w * 8;
        let raw = rgba_to_wbgr_raw(&px, bpr);
        let back = wbgr_to_rgba(&raw, w, h, bpr).unwrap();
        for (o, b) in px.rgba.iter().zip(&back.rgba) {
            assert!(
                (*o as i32 - *b as i32).abs() <= 1,
                "WBGR round-trip drifted > 1"
            );
        }
    }

    /// Parse a "RGBA"-magic .rgbaref oracle dump (w, h, premultiplied RGBA8).
    fn read_rgbaref(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
        let mut d = fs::read(path).ok()?;
        // Committed oracle dumps are LZFSE-wrapped to keep the repo small.
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

    fn premultiply(c: u8, a: u8) -> u8 {
        ((c as u32 * a as u32 + 127) / 255) as u8
    }

    /// Decode a wide-format LZFSE CELM rendition from its CSI blob;
    /// None when not decodable here (e.g. deepmap2).
    fn decode_wide_rendition(blob: &[u8]) -> Option<Pixels> {
        let csi = Csi::parse(blob).ok()?;
        let pf = csi.header.pixel_format;
        if !is_wide_format(pf) {
            return None;
        }
        let bpr = csi
            .tlv(tlv::BYTES_PER_ROW)
            .filter(|d| d.len() >= 4)
            .map(|d| u32::from_le_bytes(d[0..4].try_into().unwrap()))
            .unwrap_or_else(|| {
                let bpp = if pf == WBGR { 8 } else { 4 };
                crate::format::bytes_per_row(csi.header.width, bpp)
            });
        let expected = bpr as usize * csi.header.height as usize;
        let celm = codec::celm_decode(&csi.payload, expected).ok()?;
        let raw = celm.raw?; // None for deepmap2.
        to_rgba(&raw, csi.header.width, csi.header.height, bpr, pf)
            .ok()
            .flatten()
    }

    /// WBGR fixtures, re-premultiplied, must match CoreUI within ±2/channel
    /// (1 count un/premultiply rounding + 1 count extended-range clamp drift).
    #[test]
    fn wbgr_fixtures_match_oracle_within_tolerance() {
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
            if !name.starts_with("wbgr_") || !name.ends_with(".csi") {
                continue;
            }
            // Skip deepmap2 (comp 11) fixtures — decoded in deepmap.rs, not here.
            let blob0 = fs::read(&path).unwrap();
            if let Ok(csi) = Csi::parse(&blob0) {
                if csi.payload.len() >= 12
                    && &csi.payload[0..4] == crate::format::magic::CELM
                    && u32::from_le_bytes(csi.payload[8..12].try_into().unwrap()) == 11
                {
                    continue;
                }
            }
            let refpath = dir.join(format!("{name}.rgbaref"));
            let Some((rw, rh, oracle)) = read_rgbaref(&refpath) else {
                eprintln!("{name}: no/invalid .rgbaref, skipping");
                continue;
            };
            let blob = fs::read(&path).unwrap();
            let px = decode_wide_rendition(&blob)
                .unwrap_or_else(|| panic!("{name}: WBGR decode returned None"));
            assert_eq!((px.width, px.height), (rw, rh), "{name}: dims");
            assert_eq!(oracle.len(), (rw * rh * 4) as usize, "{name}: oracle size");

            let mut maxd = 0i32;
            for i in 0..(rw * rh) as usize {
                let a8 = px.rgba[i * 4 + 3];
                // Oracle is premultiplied; re-premultiply ours.
                let ours = [
                    premultiply(px.rgba[i * 4], a8),
                    premultiply(px.rgba[i * 4 + 1], a8),
                    premultiply(px.rgba[i * 4 + 2], a8),
                    a8,
                ];
                for c in 0..4 {
                    maxd = maxd.max((ours[c] as i32 - oracle[i * 4 + c] as i32).abs());
                }
            }
            assert!(
                maxd <= TOL,
                "{name}: max per-channel delta {maxd} exceeds {TOL}"
            );
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no wbgr fixtures checked");
        }
    }

    /// Real GA16 renditions are deepmap2, so this module's plain-CELM path
    /// returns None (passthrough) while still recognizing the format as wide.
    #[test]
    fn ga16_real_rendition_is_recognized_but_passthrough() {
        let dir = fixtures_dir();
        let path = dir.join("ga16_material_mask.csi");
        if !path.exists() {
            eprintln!("no ga16 fixture, skipping");
            return;
        }
        let blob = fs::read(&path).unwrap();
        let csi = Csi::parse(&blob).unwrap();
        assert!(
            is_wide_format(csi.header.pixel_format),
            "GA16 should be a wide format"
        );
        assert!(decode_wide_rendition(&blob).is_none());
    }

    /// GA16 layout conversion on a synthetic buffer (no plain-CELM GA16
    /// rendition exists in the sample catalogs — all are deepmap2).
    #[test]
    fn ga16_synthetic_layout_conversion() {
        let (w, h, bpr) = (3u32, 2u32, 16u32);
        let pixels = [
            [(0x8000u16, 0xffffu16)], // straight gray 0x80 -> 128, alpha 255
            [(0x4000u16, 0x8000u16)], // premult gray 0x40, alpha 0x80 -> straight ~128
            [(0x0000u16, 0x0000u16)], // transparent
            [(0xffffu16, 0xffffu16)], // white opaque
            [(0x0000u16, 0xff00u16)], // black, alpha 255
            [(0x1234u16, 0x5678u16)], // arbitrary premultiplied
        ];
        let mut raw = vec![0u8; (bpr * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let (g, a) = pixels[y * w as usize + x][0];
                let off = y * bpr as usize + x * 4;
                raw[off..off + 2].copy_from_slice(&g.to_le_bytes());
                raw[off + 2..off + 4].copy_from_slice(&a.to_le_bytes());
            }
        }
        let px = to_rgba(&raw, w, h, bpr, GA16).unwrap().unwrap();
        assert_eq!((px.width, px.height), (w, h));

        let expect = |g16: u16, a16: u16| -> [u8; 4] {
            let a8 = (a16 >> 8) as u8;
            let g8 = unpremultiply((g16 >> 8) as u8, a8);
            [g8, g8, g8, a8]
        };
        for (i, p) in pixels.iter().enumerate() {
            let (g, a) = p[0];
            let got = &px.rgba[i * 4..i * 4 + 4];
            assert_eq!(got, &expect(g, a), "pixel {i}");
            assert!(got[0] == got[1] && got[1] == got[2], "pixel {i} not gray");
        }
    }

    #[test]
    fn half_to_f32_known_values() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0x4000), 2.0);
        assert_eq!(half_to_f32(0x3800), 0.5);
        assert!((half_to_f32(0x3555) - (1.0 / 3.0)).abs() < 1e-3);
        assert!(half_to_f32(0x7c00).is_infinite());
        assert!(half_to_f32(0x7e00).is_nan());
        assert_eq!(unit_to_u8(half_to_f32(0x7e00)), 0); // NaN -> 0
        assert_eq!(unit_to_u8(half_to_f32(0x7c00)), 255); // +Inf -> clamp 255
        assert_eq!(unit_to_u8(half_to_f32(0x4000)), 255); // 2.0 -> clamp 255
    }

    #[test]
    fn is_wide_format_tags() {
        assert!(is_wide_format(WBGR));
        assert!(is_wide_format(GA16));
        assert_eq!(WBGR, 0x5247_4257);
        assert_eq!(GA16, 0x4741_3136);
        assert!(!is_wide_format(u32::from_le_bytes(*b"BGRA")));
        assert!(!is_wide_format(0));
        // Unknown formats return Ok(None), never panic.
        assert!(to_rgba(&[], 1, 1, 4, 0).unwrap().is_none());
    }
}
