//! CoreUI RLE (CELM compression type 1) decode/encode. See docs/FORMAT.md §6.1.
//!
//! Stream body (after the 16-byte "MLEC" header):
//! ```text
//!   u32 magic (=3)
//!   u32 width
//!   u32 height
//!   u32 rowOffset[height]  byte offset of each row's RLE data from body start
//!   ... per-row RLE data ...
//! ```
//! Rows decode independently from `rowOffset[r]`, stopping once exactly
//! `width` elements (2 B GA8 / 4 B BGRA) are produced — no end-of-row marker.
//! Control word (u32 LE): top byte 0x80 = fill (1 element repeated `count`×),
//! 0x00 = literal (`count` elements inline); low 24 bits = element count.
//!
//! Byte-identical rows may share a `rowOffset` — dedup, NOT an empty-row
//! marker: decode every row by re-reading from the shared offset.

use anyhow::Result;

use crate::format::pixel_format;

const FILL_FLAG: u8 = 0x80;

fn bytes_per_pixel(pf: u32) -> Option<usize> {
    match pf {
        x if x == pixel_format::ARGB => Some(4),
        x if x == pixel_format::GA8 => Some(2),
        _ => None,
    }
}

/// Decompress an RLE stream body into raw premultiplied rows.
/// Ok(None) for unknown pixel formats or inconsistent streams.
pub fn decode(
    stream: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
    pixel_format: u32,
) -> Result<Option<Vec<u8>>> {
    let Some(bpp) = bytes_per_pixel(pixel_format) else {
        return Ok(None);
    };
    let w = width as usize;
    let h = height as usize;
    let bpr = bytes_per_row as usize;
    if bpr < w * bpp {
        return Ok(None);
    }
    // header (12) + offset table (4 * height)
    let table_start = 12usize;
    let table_end = match table_start.checked_add(4usize.saturating_mul(h)) {
        Some(v) if v <= stream.len() => v,
        _ => return Ok(None),
    };
    let mut offsets = Vec::with_capacity(h);
    for r in 0..h {
        let o = table_start + 4 * r;
        offsets.push(u32::from_le_bytes(stream[o..o + 4].try_into().unwrap()) as usize);
    }

    let row_bytes = w * bpp;
    let mut out = vec![0u8; bpr * h];
    for r in 0..h {
        let start = offsets[r];
        if start < table_end || start > stream.len() {
            return Ok(None);
        }
        let rb = &stream[start..];
        let Some(row) = decode_row(rb, row_bytes, bpp) else {
            return Ok(None);
        };
        out[r * bpr..r * bpr + row_bytes].copy_from_slice(&row);
    }
    Ok(Some(out))
}

/// Decode one row from the front of `rb`, self-terminating at `row_bytes`;
/// None on overrun or truncation.
fn decode_row(rb: &[u8], row_bytes: usize, bpp: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(row_bytes);
    let mut p = 0usize;
    while out.len() < row_bytes {
        let ctrl = u32::from_le_bytes(rb.get(p..p + 4)?.try_into().unwrap());
        p += 4;
        let count = (ctrl & 0x00FF_FFFF) as usize;
        let flag = (ctrl >> 24) as u8;
        if flag == FILL_FLAG {
            let elem = rb.get(p..p + bpp)?;
            p += bpp;
            for _ in 0..count {
                out.extend_from_slice(elem);
            }
        } else {
            let lit = rb.get(p..p + count * bpp)?;
            p += count * bpp;
            out.extend_from_slice(lit);
        }
        if out.len() > row_bytes {
            return None;
        }
    }
    Some(out)
}

/// Compress raw rows into a valid RLE stream body (greedy: fill for runs >= 2,
/// else literal; byte-identical rows share one offset) — not byte-identical to
/// Apple's tokenization. None for unsupported formats or inconsistent dimensions.
pub fn encode(
    raw: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
    pixel_format: u32,
) -> Option<Vec<u8>> {
    let bpp = bytes_per_pixel(pixel_format)?;
    let w = width as usize;
    let h = height as usize;
    let bpr = bytes_per_row as usize;
    let row_bytes = w * bpp;
    if bpr < row_bytes || raw.len() != bpr * h {
        return None;
    }

    let table_start = 12usize;
    let table_end = table_start + 4 * h;
    let mut body = Vec::with_capacity(table_end + raw.len());
    body.extend_from_slice(&3u32.to_le_bytes()); // magic
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.resize(table_end, 0); // offset table placeholder, filled in below

    let mut offsets = vec![0u32; h];
    let mut seen: std::collections::HashMap<&[u8], u32> =
        std::collections::HashMap::with_capacity(h);
    for r in 0..h {
        let row = &raw[r * bpr..r * bpr + row_bytes];
        if let Some(&off) = seen.get(row) {
            offsets[r] = off;
        } else {
            let off = body.len() as u32;
            body.extend_from_slice(&encode_row(row, w, bpp));
            offsets[r] = off;
            seen.insert(row, off);
        }
    }
    for (r, off) in offsets.iter().enumerate() {
        let o = table_start + 4 * r;
        body[o..o + 4].copy_from_slice(&off.to_le_bytes());
    }
    Some(body)
}

/// Greedy-tokenize one row; counts chunked to the 24-bit control-word field.
fn encode_row(row: &[u8], width: usize, bpp: usize) -> Vec<u8> {
    const MAX_COUNT: usize = 0x00FF_FFFF;
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut lit_start = 0usize;
    while i < width {
        let elem = &row[i * bpp..(i + 1) * bpp];
        let mut run = 1usize;
        while i + run < width && &row[(i + run) * bpp..(i + run + 1) * bpp] == elem {
            run += 1;
        }
        if run >= 2 {
            if lit_start < i {
                append_literal(&mut out, row, lit_start, i, bpp);
            }
            let mut remaining = run;
            let mut pos = i;
            while remaining > 0 {
                let n = remaining.min(MAX_COUNT);
                let ctrl = ((FILL_FLAG as u32) << 24) | (n as u32);
                out.extend_from_slice(&ctrl.to_le_bytes());
                out.extend_from_slice(&row[pos * bpp..pos * bpp + bpp]);
                remaining -= n;
                pos += n;
            }
            i += run;
            lit_start = i;
        } else {
            i += 1;
        }
    }
    if lit_start < width {
        append_literal(&mut out, row, lit_start, width, bpp);
    }
    out
}

/// Append literal token(s) for elements `[start, end)`, chunked to 24-bit counts.
fn append_literal(out: &mut Vec<u8>, row: &[u8], start: usize, end: usize, bpp: usize) {
    const MAX_COUNT: usize = 0x00FF_FFFF;
    let mut s = start;
    while s < end {
        let n = (end - s).min(MAX_COUNT);
        let ctrl = n as u32; // flag byte 0x00
        out.extend_from_slice(&ctrl.to_le_bytes());
        out.extend_from_slice(&row[s * bpp..(s + n) * bpp]);
        s += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_fixtures")
    }

    #[test]
    fn rle_decode_matches_reference_byte_exact() {
        let dir = fixtures_dir();
        if !dir.is_dir() {
            eprintln!("no re_fixtures dir, skipping");
            return;
        }
        let mut checked = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("rle_") || !name.ends_with(".csi") {
                continue;
            }
            let blob = fs::read(&path).unwrap();
            let csi = crate::csi::Csi::parse(&blob).unwrap();
            let bpr = csi
                .tlv(crate::format::tlv::BYTES_PER_ROW)
                .map(|d| u32::from_le_bytes(d[0..4].try_into().unwrap()))
                .unwrap_or_else(|| {
                    let bpp = if csi.header.pixel_format == pixel_format::GA8 {
                        2
                    } else {
                        4
                    };
                    crate::format::bytes_per_row(csi.header.width, bpp)
                });
            // CELM stream body = payload after the 16-byte MLEC header.
            let stream = &csi.payload[16..];
            let raw = decode(
                stream,
                csi.header.width,
                csi.header.height,
                bpr,
                csi.header.pixel_format,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .unwrap_or_else(|| panic!("{name}: rle decode returned None"));
            assert_eq!(raw.len(), bpr as usize * csi.header.height as usize);

            let refpath = dir.join(format!("{name}.rawref"));
            if refpath.exists() {
                let mut reference = fs::read(&refpath).unwrap();
                // Committed references are LZFSE-wrapped to keep the repo small.
                if reference.len() >= 3 && &reference[0..3] == b"bvx" {
                    reference = crate::codec::lzfse_decode(&reference).unwrap();
                }
                assert_eq!(raw, reference, "{name}: RLE raw rows differ from reference");
            } else {
                eprintln!("{name}: no .rawref, checked structure only");
            }
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no rle fixtures checked");
        }
    }

    #[test]
    fn synthetic_rle_round_trips_through_decode() {
        // Hand-built 2x2 GA8 body: row0 fill, row1 literal.
        let width = 2u32;
        let height = 2u32;
        let bpp = 2usize;
        let bpr = 4u32; // 2px * 2bpp, no padding
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_le_bytes()); // magic
        body.extend_from_slice(&width.to_le_bytes());
        body.extend_from_slice(&height.to_le_bytes());
        let table_pos = body.len();
        body.extend_from_slice(&[0u8; 8]);
        let row0_off = body.len() as u32;
        // fill: ctrl = 0x80<<24 | count(2), then 1 element
        body.extend_from_slice(&(0x8000_0000u32 | 2).to_le_bytes());
        body.extend_from_slice(&[0x10, 0xff]);
        let row1_off = body.len() as u32;
        // literal: ctrl = count(2), then 2 elements
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&[0x20, 0x01, 0x30, 0x02]);
        body[table_pos..table_pos + 4].copy_from_slice(&row0_off.to_le_bytes());
        body[table_pos + 4..table_pos + 8].copy_from_slice(&row1_off.to_le_bytes());

        let raw = decode(&body, width, height, bpr, pixel_format::GA8)
            .unwrap()
            .expect("synthetic rle should decode");
        assert_eq!(
            raw,
            vec![
                0x10, 0xff, 0x10, 0xff, /* row0 */ 0x20, 0x01, 0x30, 0x02 /* row1 */
            ],
        );
        let _ = bpp;
    }

    fn decode_ok(stream: &[u8], width: u32, height: u32, bpr: u32, pf: u32) -> Vec<u8> {
        decode(stream, width, height, bpr, pf)
            .unwrap()
            .expect("should decode")
    }

    #[test]
    fn encode_decode_round_trips_ga8_with_runs_and_literals() {
        let width = 4u32;
        let height = 3u32;
        let bpp = 2usize;
        let bpr = width as usize * bpp;
        let mut raw = Vec::new();
        // row0: fill (0x42, 0x80) x4
        for _ in 0..4 {
            raw.extend_from_slice(&[0x42, 0x80]);
        }
        // row1: 4 distinct elements
        for i in 0..4u8 {
            raw.extend_from_slice(&[i * 10 + 1, i * 10 + 2]);
        }
        // row2: run of 3 identical + 1 distinct
        for _ in 0..3 {
            raw.extend_from_slice(&[0x11, 0x22]);
        }
        raw.extend_from_slice(&[0x33, 0x44]);

        let body = encode(&raw, width, height, bpr as u32, pixel_format::GA8)
            .expect("encode should succeed");
        let decoded = decode_ok(&body, width, height, bpr as u32, pixel_format::GA8);
        assert_eq!(decoded, raw);
    }

    #[test]
    fn encode_decode_round_trips_bgra_with_transparent_rows() {
        let width = 5u32;
        let height = 4u32;
        let bpp = 4usize;
        let bpr = width as usize * bpp;
        let mut raw = vec![0u8; bpr * height as usize];
        // row0: fully transparent (stays all zero).
        // row1: fill run.
        for x in 0..width as usize {
            raw[bpr + x * bpp..bpr + x * bpp + 4].copy_from_slice(&[10, 20, 30, 255]);
        }
        // row2: fully transparent again.
        // row3: literal, all distinct pixels.
        for x in 0..width as usize {
            let v = x as u8;
            raw[3 * bpr + x * bpp..3 * bpr + x * bpp + 4].copy_from_slice(&[
                v,
                v + 1,
                v + 2,
                v + 3,
            ]);
        }

        let body = encode(&raw, width, height, bpr as u32, pixel_format::ARGB)
            .expect("encode should succeed");
        let decoded = decode_ok(&body, width, height, bpr as u32, pixel_format::ARGB);
        assert_eq!(decoded, raw);
    }

    #[test]
    fn encode_decode_round_trips_single_pixel_rows() {
        let width = 1u32;
        let height = 3u32;
        let bpp = 2usize;
        let bpr = width as usize * bpp;
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0u8, 0u8]); // transparent
        raw.extend_from_slice(&[0x55, 0xaa]); // opaque single pixel
        raw.extend_from_slice(&[0xff, 0x01]);

        let body = encode(&raw, width, height, bpr as u32, pixel_format::GA8)
            .expect("encode should succeed");
        let decoded = decode_ok(&body, width, height, bpr as u32, pixel_format::GA8);
        assert_eq!(decoded, raw);
    }

    #[test]
    fn encode_decode_round_trips_fully_transparent_image() {
        let width = 6u32;
        let height = 5u32;
        let bpp = 4usize;
        let bpr = width as usize * bpp;
        let raw = vec![0u8; bpr * height as usize];
        let body = encode(&raw, width, height, bpr as u32, pixel_format::ARGB)
            .expect("encode should succeed");
        let decoded = decode_ok(&body, width, height, bpr as u32, pixel_format::ARGB);
        assert_eq!(decoded, raw);
        // All-zero rows dedup to a single encoded row shared by every offset entry.
        let expected_row_bytes = 4 + bpp; // one fill control word + one element
        assert_eq!(body.len(), 12 + 4 * height as usize + expected_row_bytes);
        for r in 1..height as usize {
            let o = 12 + 4 * r;
            let off_r = u32::from_le_bytes(body[o..o + 4].try_into().unwrap());
            let off_0 = u32::from_le_bytes(body[12..16].try_into().unwrap());
            assert_eq!(off_r, off_0, "row {r} should dedup to row 0's offset");
        }
    }

    #[test]
    fn encode_declines_unsupported_pixel_format() {
        assert!(encode(&[0u8; 8], 2, 2, 4, 0xdead_beef).is_none());
    }

    #[test]
    fn encode_declines_inconsistent_length() {
        assert!(encode(&[0u8; 7], 2, 2, 4, pixel_format::GA8).is_none());
    }

    #[test]
    fn real_rle_fixture_round_trips_through_encode() {
        let dir = fixtures_dir();
        if !dir.is_dir() {
            eprintln!("no re_fixtures dir, skipping");
            return;
        }
        let mut checked = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("rle_") || !name.ends_with(".csi") {
                continue;
            }
            let blob = fs::read(&path).unwrap();
            let csi = crate::csi::Csi::parse(&blob).unwrap();
            let bpr = csi
                .tlv(crate::format::tlv::BYTES_PER_ROW)
                .map(|d| u32::from_le_bytes(d[0..4].try_into().unwrap()))
                .unwrap_or_else(|| {
                    let bpp = if csi.header.pixel_format == pixel_format::GA8 {
                        2
                    } else {
                        4
                    };
                    crate::format::bytes_per_row(csi.header.width, bpp)
                });
            let stream = &csi.payload[16..];
            let raw = decode_ok(
                stream,
                csi.header.width,
                csi.header.height,
                bpr,
                csi.header.pixel_format,
            );

            let reencoded = encode(
                &raw,
                csi.header.width,
                csi.header.height,
                bpr,
                csi.header.pixel_format,
            )
            .unwrap_or_else(|| panic!("{name}: encode should succeed"));
            let redecoded = decode_ok(
                &reencoded,
                csi.header.width,
                csi.header.height,
                bpr,
                csi.header.pixel_format,
            );
            assert_eq!(redecoded, raw, "{name}: decode(encode(raw)) != raw");
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no rle fixtures checked");
        }
    }
}
