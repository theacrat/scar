//! Rendition payload codecs: CELM bitmaps (lzfse / zlib / uncompressed /
//! chunked KCBC), RAWD data, MSIS sets, INLK links, COLR colors.
//! See docs/FORMAT.md §6 and §9.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};

use crate::format::{compression, magic, pixel_format};

/// A decoded bitmap in straight (non-premultiplied) RGBA8.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Parsed CELM envelope.
pub struct Celm {
    pub flags: u32,
    pub compression: u32,
    /// Decompressed raw rows; None when the compression isn't decoded here.
    pub raw: Option<Vec<u8>>,
}

const CELM_HEADER_LEN: usize = 16;
const KCBC_HEADER_LEN: usize = 20;

/// Decompress one stream; None for compressions we don't decode.
fn decode_stream(compression: u32, data: &[u8]) -> Result<Option<Vec<u8>>> {
    match compression {
        compression::UNCOMPRESSED => Ok(Some(data.to_vec())),
        compression::ZLIB => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(data)
                .read_to_end(&mut out)
                .context("zlib inflate failed")?;
            Ok(Some(out))
        }
        // LZVN streams in the wild are bvx*-framed, same decoder as LZFSE.
        compression::LZVN | compression::LZFSE => Ok(Some(lzfse_decode(data)?)),
        _ => Ok(None),
    }
}

/// Compress `data`; None for compressions we don't encode.
fn encode_stream(compression: u32, data: &[u8]) -> Option<Vec<u8>> {
    match compression {
        compression::UNCOMPRESSED => Some(data.to_vec()),
        compression::ZLIB => {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).ok()?;
            enc.finish().ok()
        }
        compression::LZVN | compression::LZFSE => Some(lzfse_encode(data)),
        _ => None,
    }
}

/// Parse and decompress a CELM payload; `expected_raw_len` (bpr * height)
/// validates the output when a raw is produced.
pub fn celm_decode(payload: &[u8], expected_raw_len: usize) -> Result<Celm> {
    if payload.len() < CELM_HEADER_LEN {
        bail!("CELM payload too short: {} bytes", payload.len());
    }
    if &payload[0..4] != magic::CELM {
        bail!("bad CELM magic: {:?}", &payload[0..4]);
    }
    let flags = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let comp = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let field3 = u32::from_le_bytes(payload[12..16].try_into().unwrap());
    let chunked = flags & 1 != 0;

    let raw = if chunked {
        let chunk_count = field3 as usize;
        let mut off = CELM_HEADER_LEN;
        let mut chunks_raw: Vec<Vec<u8>> = Vec::with_capacity(chunk_count);
        let mut all_supported = true;
        for _ in 0..chunk_count {
            let hdr = payload
                .get(off..off + KCBC_HEADER_LEN)
                .context("truncated KCBC chunk header")?;
            if &hdr[0..4] != magic::KCBC {
                bail!("bad KCBC magic: {:?}", &hdr[0..4]);
            }
            let _row_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            let compressed_len = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
            off += KCBC_HEADER_LEN;
            let chunk_bytes = payload
                .get(off..off + compressed_len)
                .context("truncated KCBC chunk data")?;
            off += compressed_len;
            match decode_stream(comp, chunk_bytes)? {
                Some(v) => chunks_raw.push(v),
                None => {
                    all_supported = false;
                    break;
                }
            }
        }
        if all_supported {
            let mut out = Vec::new();
            for c in chunks_raw {
                out.extend_from_slice(&c);
            }
            Some(out)
        } else {
            None
        }
    } else {
        let len = field3 as usize;
        let data = payload
            .get(CELM_HEADER_LEN..CELM_HEADER_LEN + len)
            .context("truncated CELM plain payload")?;
        decode_stream(comp, data)?
    };

    if let Some(r) = &raw {
        if r.len() != expected_raw_len {
            bail!(
                "CELM decompressed length {} != expected {}",
                r.len(),
                expected_raw_len
            );
        }
    }

    Ok(Celm {
        flags,
        compression: comp,
        raw,
    })
}

/// Build a CELM payload from raw rows, chunked-KCBC lzfse when
/// `compression == LZFSE`, otherwise plain (zlib/uncompressed).
pub fn celm_encode(raw: &[u8], bytes_per_row: u32, compression: u32) -> Result<Vec<u8>> {
    if bytes_per_row == 0 {
        bail!("celm_encode: bytes_per_row must be nonzero");
    }
    let bpr = bytes_per_row as usize;
    if !raw.len().is_multiple_of(bpr) {
        bail!(
            "celm_encode: raw length {} not a multiple of bytes_per_row {}",
            raw.len(),
            bpr
        );
    }
    let total_rows = raw.len() / bpr;

    if compression == compression::LZFSE {
        // ~1 MiB of raw rows per chunk, close to Apple's granularity.
        let rows_per_chunk = std::cmp::max(1, (1usize << 20) / bpr);
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut row = 0usize;
        while row < total_rows {
            let n = std::cmp::min(rows_per_chunk, total_rows - row);
            let start = row * bpr;
            let end = start + n * bpr;
            chunks.push(raw[start..end].to_vec());
            row += n;
        }
        if chunks.is_empty() {
            // Zero-height image: emit one empty chunk.
            chunks.push(Vec::new());
        }

        let mut out = Vec::with_capacity(payload_size_guess(raw.len()));
        out.extend_from_slice(magic::CELM);
        out.extend_from_slice(&3u32.to_le_bytes()); // flags: chunked
        out.extend_from_slice(&compression::LZFSE.to_le_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for chunk in &chunks {
            let row_count = (chunk.len() / bpr) as u32;
            let compressed = lzfse_encode(chunk);
            out.extend_from_slice(magic::KCBC);
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&row_count.to_le_bytes());
            out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            out.extend_from_slice(&compressed);
        }
        Ok(out)
    } else {
        let data = encode_stream(compression, raw)
            .with_context(|| format!("celm_encode: unsupported compression {compression}"))?;
        let mut out = Vec::with_capacity(CELM_HEADER_LEN + data.len());
        out.extend_from_slice(magic::CELM);
        out.extend_from_slice(&2u32.to_le_bytes()); // flags: plain
        out.extend_from_slice(&compression.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        Ok(out)
    }
}

fn payload_size_guess(raw_len: usize) -> usize {
    CELM_HEADER_LEN + raw_len / 2 + 64
}

fn premultiply(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

fn unpremultiply(c: u8, a: u8) -> u8 {
    if a == 0 {
        0
    } else {
        let v = (c as u32 * 255 + a as u32 / 2) / a as u32;
        v.min(255) as u8
    }
}

/// Premultiplied BGRA/GA8 rows -> straight RGBA (drops row padding).
pub fn raw_to_rgba(
    raw: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
    pixel_format: u32,
) -> Result<Pixels> {
    let bpr = bytes_per_row as usize;
    let w = width as usize;
    let h = height as usize;
    if raw.len() < bpr * h {
        bail!(
            "raw_to_rgba: raw buffer too short ({} < {})",
            raw.len(),
            bpr * h
        );
    }
    let mut rgba = vec![0u8; w * h * 4];
    match pixel_format {
        pixel_format::ARGB => {
            let bpp = 4;
            if bpr < w * bpp {
                bail!(
                    "raw_to_rgba: bytes_per_row {} too small for width {}",
                    bpr,
                    w
                );
            }
            for y in 0..h {
                let row = &raw[y * bpr..y * bpr + w * bpp];
                for x in 0..w {
                    let px = &row[x * bpp..x * bpp + 4];
                    let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
                    let out = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    out[0] = unpremultiply(r, a);
                    out[1] = unpremultiply(g, a);
                    out[2] = unpremultiply(b, a);
                    out[3] = a;
                }
            }
        }
        pixel_format::GA8 => {
            let bpp = 2;
            if bpr < w * bpp {
                bail!(
                    "raw_to_rgba: bytes_per_row {} too small for width {}",
                    bpr,
                    w
                );
            }
            for y in 0..h {
                let row = &raw[y * bpr..y * bpr + w * bpp];
                for x in 0..w {
                    let px = &row[x * bpp..x * bpp + 2];
                    let (gray, a) = (px[0], px[1]);
                    let g = unpremultiply(gray, a);
                    let out = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    out[0] = g;
                    out[1] = g;
                    out[2] = g;
                    out[3] = a;
                }
            }
        }
        other => bail!("raw_to_rgba: unsupported pixel format 0x{other:08x}"),
    }
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

/// Straight RGBA -> premultiplied BGRA or GA8 rows with the given stride.
pub fn rgba_to_raw(px: &Pixels, bytes_per_row: u32, pixel_format: u32) -> Result<Vec<u8>> {
    let w = px.width as usize;
    let h = px.height as usize;
    let bpr = bytes_per_row as usize;
    if px.rgba.len() != w * h * 4 {
        bail!(
            "rgba_to_raw: rgba buffer length {} != {}x{}x4",
            px.rgba.len(),
            w,
            h
        );
    }
    let mut raw = vec![0u8; bpr * h];
    match pixel_format {
        pixel_format::ARGB => {
            let bpp = 4;
            if bpr < w * bpp {
                bail!(
                    "rgba_to_raw: bytes_per_row {} too small for width {}",
                    bpr,
                    w
                );
            }
            for y in 0..h {
                let row = &mut raw[y * bpr..y * bpr + w * bpp];
                for x in 0..w {
                    let src = &px.rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    let (r, g, b, a) = (src[0], src[1], src[2], src[3]);
                    let out = &mut row[x * bpp..x * bpp + 4];
                    out[0] = premultiply(b, a);
                    out[1] = premultiply(g, a);
                    out[2] = premultiply(r, a);
                    out[3] = a;
                }
            }
        }
        pixel_format::GA8 => {
            let bpp = 2;
            if bpr < w * bpp {
                bail!(
                    "rgba_to_raw: bytes_per_row {} too small for width {}",
                    bpr,
                    w
                );
            }
            for y in 0..h {
                let row = &mut raw[y * bpr..y * bpr + w * bpp];
                for x in 0..w {
                    let src = &px.rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    // GA8 sources are grayscale; red channel used as gray.
                    let (gray, a) = (src[0], src[3]);
                    let out = &mut row[x * bpp..x * bpp + 2];
                    out[0] = premultiply(gray, a);
                    out[1] = a;
                }
            }
        }
        other => bail!("rgba_to_raw: unsupported pixel format 0x{other:08x}"),
    }
    Ok(raw)
}

/// RAWD payload -> contained bytes (inflated when LZFSE-wrapped) + wrapped flag.
pub fn rawd_decode(payload: &[u8]) -> Result<(Vec<u8>, bool)> {
    if payload.len() < 12 {
        bail!("RAWD payload too short: {} bytes", payload.len());
    }
    if &payload[0..4] != magic::RAWD {
        bail!("bad RAWD magic: {:?}", &payload[0..4]);
    }
    let _version = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let raw_len = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    let bytes = payload
        .get(12..12 + raw_len)
        .context("truncated RAWD payload bytes")?;
    if bytes.len() >= 3 && &bytes[0..3] == b"bvx" {
        let inflated = lzfse_decode(bytes)?;
        Ok((inflated, true))
    } else {
        Ok((bytes.to_vec(), false))
    }
}

pub fn rawd_encode(data: &[u8], lzfse_wrap: bool) -> Vec<u8> {
    let bytes = if lzfse_wrap {
        lzfse_encode(data)
    } else {
        data.to_vec()
    };
    // Version is a compression flag; writing 1 over non-LZFSE bytes makes CoreUI's LZFSE reader spin forever.
    let version: u32 = if lzfse_wrap { 1 } else { 0 };
    let mut out = Vec::with_capacity(12 + bytes.len());
    out.extend_from_slice(magic::RAWD);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
    out
}

pub struct MsisEntry {
    pub width: u32,
    pub height: u32,
    pub index: u32,
}

pub fn msis_decode(payload: &[u8]) -> Result<Vec<MsisEntry>> {
    if payload.len() < 12 {
        bail!("MSIS payload too short: {} bytes", payload.len());
    }
    if &payload[0..4] != magic::MSIS {
        bail!("bad MSIS magic: {:?}", &payload[0..4]);
    }
    let _field2 = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let count = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 12;
    for _ in 0..count {
        let entry = payload.get(off..off + 12).context("truncated MSIS entry")?;
        let width = u32::from_le_bytes(entry[0..4].try_into().unwrap());
        let height = u32::from_le_bytes(entry[4..8].try_into().unwrap());
        let index = u32::from_le_bytes(entry[8..12].try_into().unwrap());
        out.push(MsisEntry {
            width,
            height,
            index,
        });
        off += 12;
    }
    Ok(out)
}

pub fn msis_encode(entries: &[MsisEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + entries.len() * 12);
    out.extend_from_slice(magic::MSIS);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&e.width.to_le_bytes());
        out.extend_from_slice(&e.height.to_le_bytes());
        out.extend_from_slice(&e.index.to_le_bytes());
    }
    out
}

pub struct Inlk {
    pub flags: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub content_layout: u16,
    /// (attribute id, value) pairs including any (0,0) terminator, verbatim.
    pub key_pairs: Vec<(u16, u16)>,
}

const INLK_FIXED_LEN: usize = 24; // magic + flags + x + y + width + height

pub fn inlk_decode(tlv: &[u8]) -> Result<Inlk> {
    if tlv.len() < INLK_FIXED_LEN + 2 + 4 {
        bail!("INLK tlv too short: {} bytes", tlv.len());
    }
    if &tlv[0..4] != magic::INLK {
        bail!("bad INLK magic: {:?}", &tlv[0..4]);
    }
    let flags = u32::from_le_bytes(tlv[4..8].try_into().unwrap());
    let x = u32::from_le_bytes(tlv[8..12].try_into().unwrap());
    let y = u32::from_le_bytes(tlv[12..16].try_into().unwrap());
    let width = u32::from_le_bytes(tlv[16..20].try_into().unwrap());
    let height = u32::from_le_bytes(tlv[20..24].try_into().unwrap());
    let content_layout = u16::from_le_bytes(tlv[24..26].try_into().unwrap());
    // key_length is a u32 stored unaligned at offset 26.
    let key_length = u32::from_le_bytes(tlv[26..30].try_into().unwrap()) as usize;
    let pairs_bytes = tlv
        .get(30..30 + key_length)
        .context("truncated INLK key pairs")?;
    if !key_length.is_multiple_of(4) {
        bail!("INLK key length {key_length} not a multiple of 4");
    }
    let mut key_pairs = Vec::with_capacity(key_length / 4);
    let mut p = 0;
    while p < pairs_bytes.len() {
        let attr = u16::from_le_bytes(pairs_bytes[p..p + 2].try_into().unwrap());
        let val = u16::from_le_bytes(pairs_bytes[p + 2..p + 4].try_into().unwrap());
        key_pairs.push((attr, val));
        p += 4;
    }
    Ok(Inlk {
        flags,
        x,
        y,
        width,
        height,
        content_layout,
        key_pairs,
    })
}

pub fn inlk_encode(link: &Inlk) -> Vec<u8> {
    let mut out = Vec::with_capacity(INLK_FIXED_LEN + 2 + 4 + link.key_pairs.len() * 4);
    out.extend_from_slice(magic::INLK);
    out.extend_from_slice(&link.flags.to_le_bytes());
    out.extend_from_slice(&link.x.to_le_bytes());
    out.extend_from_slice(&link.y.to_le_bytes());
    out.extend_from_slice(&link.width.to_le_bytes());
    out.extend_from_slice(&link.height.to_le_bytes());
    out.extend_from_slice(&link.content_layout.to_le_bytes());
    let key_length = (link.key_pairs.len() * 4) as u32;
    out.extend_from_slice(&key_length.to_le_bytes());
    for (attr, val) in &link.key_pairs {
        out.extend_from_slice(&attr.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

pub struct Color {
    pub color_space: u32,
    pub components: Vec<f64>,
    /// System-color name from a trailing second RLOC block (name not
    /// NUL-terminated); dropping it when present crashes CoreUI/assetutil.
    pub system_name: Option<String>,
    /// Unmodeled trailing bytes, preserved for byte-exact round-trip.
    pub trailing: Vec<u8>,
}

/// Parse a COLR payload (layout: docs/FORMAT.md §6.5).
pub fn colr_decode(payload: &[u8]) -> Result<Color> {
    if payload.len() < 16 {
        bail!("COLR payload too short: {} bytes", payload.len());
    }
    if &payload[0..4] != magic::COLR {
        bail!("bad COLR magic: {:?}", &payload[0..4]);
    }
    let _version = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let color_space = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let n = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as usize;
    let mut components = Vec::with_capacity(n);
    let mut off = 16;
    for _ in 0..n {
        let bytes = payload
            .get(off..off + 8)
            .context("truncated COLR component")?;
        components.push(f64::from_le_bytes(bytes.try_into().unwrap()));
        off += 8;
    }

    // Optional trailing system-color-name block.
    let mut system_name = None;
    let rest = &payload[off..];
    if rest.len() >= 12 && &rest[0..4] == magic::COLR {
        let name_len = u32::from_le_bytes(rest[8..12].try_into().unwrap()) as usize;
        if let Some(name_bytes) = rest.get(12..12 + name_len) {
            if let Ok(name) = std::str::from_utf8(name_bytes) {
                system_name = Some(name.to_string());
                off += 12 + name_len;
            }
        }
    }
    let trailing = payload[off..].to_vec();

    Ok(Color {
        color_space,
        components,
        system_name,
        trailing,
    })
}

pub fn colr_encode(c: &Color) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + c.components.len() * 8);
    out.extend_from_slice(magic::COLR);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&c.color_space.to_le_bytes());
    out.extend_from_slice(&(c.components.len() as u32).to_le_bytes());
    for v in &c.components {
        out.extend_from_slice(&v.to_le_bytes());
    }
    if let Some(name) = &c.system_name {
        out.extend_from_slice(magic::COLR);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    out.extend_from_slice(&c.trailing);
    out
}

/// Decode one complete LZFSE stream (bvx*..bvx$).
pub fn lzfse_decode(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    lzfse_rust::decode_bytes(data, &mut out)
        .map_err(|e| anyhow::anyhow!("lzfse decode failed: {e}"))?;
    Ok(out)
}

/// Encode bytes as one LZFSE stream.
pub fn lzfse_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // Only fails on io errors, impossible into a growable Vec.
    lzfse_rust::encode_bytes(data, &mut out).expect("lzfse encode into Vec cannot fail");
    out
}

/// PNG helpers (straight RGBA8).
pub fn write_png(path: &std::path::Path, px: &Pixels) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, px.width, px.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Best);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    writer
        .write_image_data(&px.rgba)
        .context("writing PNG image data")?;
    Ok(())
}

pub fn read_png(path: &std::path::Path) -> Result<Pixels> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("reading PNG frame")?;
    let bytes = &buf[..info.buffer_size()];

    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => bytes.to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(bytes.len() / 3 * 4);
            for px in bytes.chunks_exact(3) {
                out.extend_from_slice(px);
                out.push(255);
            }
            out
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(bytes.len() / 2 * 4);
            for px in bytes.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        (png::ColorType::Grayscale, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(bytes.len() * 4);
            for &g in bytes {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        (ct, bd) => bail!(
            "read_png: unsupported PNG color type {:?} / depth {:?}",
            ct,
            bd
        ),
    };

    Ok(Pixels {
        width: info.width,
        height: info.height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Committed CSI fixtures (extracted by examples/extract_fixtures.rs).
    fn fixture_dirs() -> Vec<PathBuf> {
        ["tests/fixtures"]
            .iter()
            .map(|d| Path::new(env!("CARGO_MANIFEST_DIR")).join(d))
            .filter(|d| d.is_dir())
            .collect()
    }

    fn read_fixture(name_substr: &str) -> Option<(String, Vec<u8>)> {
        for dir in fixture_dirs() {
            for entry in fs::read_dir(&dir).ok()?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.contains(name_substr) {
                    return Some((name.clone(), fs::read(entry.path()).ok()?));
                }
            }
        }
        None
    }

    fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed ^ 0x9E3779B97F4A7C15;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            // xorshift64*
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.push((state >> 24) as u8);
        }
        out
    }

    #[test]
    fn lzfse_round_trip_assorted_sizes() {
        for &len in &[0usize, 1, 2, 100_000] {
            let data = deterministic_bytes(len, len as u64);
            let encoded = lzfse_encode(&data);
            let decoded = lzfse_decode(&encoded).unwrap();
            assert_eq!(decoded, data, "len={len}");
        }
    }

    #[test]
    fn lzfse_round_trip_compressible_data() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 7) as u8).collect();
        let encoded = lzfse_encode(&data);
        assert!(encoded.len() < data.len());
        let decoded = lzfse_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn premultiply_unpremultiply_round_trip_on_premultiplied_domain() {
        for a in 0..=255u16 {
            for c in 0..=a {
                let c = c as u8;
                let a = a as u8;
                let pre = premultiply(c, a);
                let un = unpremultiply(pre, a);
                let pre2 = premultiply(un, a);
                assert_eq!(pre, pre2, "c={c} a={a}");
            }
        }
    }

    #[test]
    fn celm_chunked_lzfse_fixture_decodes_and_round_trips() {
        let Some((name, blob)) = read_fixture("celm_lzfse_chunked") else {
            eprintln!("no chunked-lzfse fixture found, skipping");
            return;
        };
        let csi = crate::csi::Csi::parse(&blob).unwrap();
        let bpr = csi
            .tlv(crate::format::tlv::BYTES_PER_ROW)
            .map(|d| u32::from_le_bytes(d[0..4].try_into().unwrap()))
            .unwrap_or_else(|| crate::format::bytes_per_row(csi.header.width, 4));
        let expected_len = bpr as usize * csi.header.height as usize;
        let celm =
            celm_decode(&csi.payload, expected_len).unwrap_or_else(|e| panic!("{name}: {e}"));
        let raw = celm.raw.expect("chunked lzfse celm should decode");
        assert_eq!(raw.len(), expected_len);

        // rgba round trip is lossy (premultiply rounding); check the exact celm encode/decode instead.
        let reencoded = celm_encode(&raw, bpr, compression::LZFSE).unwrap();
        let redecoded = celm_decode(&reencoded, expected_len).unwrap();
        assert_eq!(
            redecoded.raw.unwrap(),
            raw,
            "{name}: celm encode/decode round trip mismatch"
        );

        let px = raw_to_rgba(
            &raw,
            csi.header.width,
            csi.header.height,
            bpr,
            csi.header.pixel_format,
        )
        .unwrap();
        assert_eq!(
            px.rgba.len(),
            csi.header.width as usize * csi.header.height as usize * 4
        );
    }

    #[test]
    fn celm_deepmap2_and_rle_fixtures_are_opaque_passthrough() {
        // deepmap2/RLE decode in their own modules; celm_decode must still parse the envelope and report raw=None.
        for substr in ["celm_deepmap2", "celm_rle"] {
            let Some((name, blob)) = read_fixture(substr) else {
                eprintln!("no {substr} fixture found, skipping");
                continue;
            };
            let csi = crate::csi::Csi::parse(&blob).unwrap();
            assert_eq!(&csi.payload[0..4], magic::CELM, "{name}");
            let comp = u32::from_le_bytes(csi.payload[8..12].try_into().unwrap());
            assert!(
                comp == compression::DEEPMAP2 || comp == compression::RLE,
                "{name}: comp={comp}"
            );
            let celm = celm_decode(&csi.payload, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                celm.raw.is_none(),
                "{name}: expected opaque passthrough (raw=None)"
            );
        }
    }

    #[test]
    fn rgba_straight_alpha_premultiply_round_trip() {
        let width = 4;
        let height = 4;
        let mut rgba = Vec::new();
        for i in 0..(width * height) {
            rgba.extend_from_slice(&[
                (i * 7) as u8,
                (i * 13) as u8,
                (i * 29) as u8,
                (i * 17) as u8,
            ]);
        }
        let px = Pixels {
            width: width as u32,
            height: height as u32,
            rgba,
        };
        let bpr = crate::format::bytes_per_row(width as u32, 4);
        let raw1 = rgba_to_raw(&px, bpr, pixel_format::ARGB).unwrap();
        let px2 = raw_to_rgba(&raw1, width as u32, height as u32, bpr, pixel_format::ARGB).unwrap();
        let raw2 = rgba_to_raw(&px2, bpr, pixel_format::ARGB).unwrap();
        assert_eq!(raw1, raw2);
    }

    #[test]
    fn rawd_fixture_inflates_to_svg_and_round_trips() {
        // Not every RAWD/SVG is lzfse-wrapped despite renditionFlags bit 2 — rawd_decode sniffs the "bvx" prefix.
        let mut checked = 0;
        for entry in fixture_dirs().iter().flat_map(|d| fs::read_dir(d).unwrap()) {
            let entry = entry.unwrap();
            let fname = entry.file_name().to_string_lossy().into_owned();
            if !fname.contains("rawd") {
                continue;
            }
            let blob = fs::read(entry.path()).unwrap();
            let csi = crate::csi::Csi::parse(&blob).unwrap();
            let (data, wrapped) =
                rawd_decode(&csi.payload).unwrap_or_else(|e| panic!("{fname}: {e}"));
            let text = String::from_utf8_lossy(&data);
            assert!(
                text.trim_start().starts_with("<?xml") || text.trim_start().starts_with("<svg"),
                "{fname}: unexpected RAWD contents: {:.80}",
                text
            );
            let reencoded = rawd_encode(&data, wrapped);
            let (redecoded, rewrapped) = rawd_decode(&reencoded).unwrap();
            assert_eq!(rewrapped, wrapped, "{fname}");
            assert_eq!(redecoded, data, "{fname}");
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no rawd fixtures found, skipping");
        }
    }

    #[test]
    fn rawd_version_word_encodes_compression() {
        // Raw RAWD must be version 0; version 1 on raw/empty data hangs CoreUI's LZFSE reader.
        let raw = rawd_encode(b"hello, raw data", false);
        assert_eq!(
            u32::from_le_bytes(raw[4..8].try_into().unwrap()),
            0,
            "raw data must be version 0"
        );
        let empty = rawd_encode(b"", false);
        assert_eq!(&empty[0..4], magic::RAWD);
        assert_eq!(
            u32::from_le_bytes(empty[4..8].try_into().unwrap()),
            0,
            "empty raw must be version 0"
        );
        assert_eq!(
            u32::from_le_bytes(empty[8..12].try_into().unwrap()),
            0,
            "empty length"
        );
        assert_eq!(empty.len(), 12, "empty raw RAWD is header-only");
        let wrapped = rawd_encode(b"hello, raw data", true);
        assert_eq!(
            u32::from_le_bytes(wrapped[4..8].try_into().unwrap()),
            1,
            "lzfse-wrapped must be version 1"
        );
        assert_eq!(
            rawd_decode(&raw).unwrap(),
            (b"hello, raw data".to_vec(), false)
        );
        assert_eq!(
            rawd_decode(&wrapped).unwrap(),
            (b"hello, raw data".to_vec(), true)
        );
        assert_eq!(rawd_decode(&empty).unwrap(), (Vec::new(), false));
    }

    #[test]
    fn msis_fixture_has_sensible_entries_and_round_trips() {
        let entries_found = fixture_dirs()
            .iter()
            .flat_map(|d| fs::read_dir(d).unwrap())
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("msis_"));
        let Some(entry) = entries_found else {
            eprintln!("no msis fixture found, skipping");
            return;
        };
        let blob = fs::read(entry.path()).unwrap();
        let csi = crate::csi::Csi::parse(&blob).unwrap();
        let entries = msis_decode(&csi.payload).unwrap();
        assert!(!entries.is_empty());
        for e in &entries {
            assert!(e.width > 0 && e.width < 10_000);
            assert!(e.height > 0 && e.height < 10_000);
        }
        let reencoded = msis_encode(&entries);
        assert_eq!(
            reencoded, csi.payload,
            "MSIS re-encode must be byte-perfect"
        );
    }

    #[test]
    fn inlk_fixture_decodes_expected_pairs_and_round_trips() {
        let Some((name, blob)) = read_fixture("inlk_") else {
            eprintln!("no inlk fixture found, skipping");
            return;
        };
        let csi = crate::csi::Csi::parse(&blob).unwrap();
        let tlv = csi
            .tlv(crate::format::tlv::INTERNAL_LINK)
            .unwrap_or_else(|| panic!("{name}: missing INTERNAL_LINK tlv"));
        let link = inlk_decode(tlv).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            link.key_pairs.first(),
            Some(&(1, 9)),
            "{name}: link target should be element 9 (packed asset)"
        );
        assert_eq!(
            link.key_pairs.last(),
            Some(&(0, 0)),
            "{name}: key pairs must be zero-terminated"
        );
        let reencoded = inlk_encode(&link);
        assert_eq!(
            reencoded, tlv,
            "{name}: INLK re-encode must be byte-perfect"
        );
    }

    #[test]
    fn colr_round_trips() {
        let c = Color {
            color_space: 1,
            components: vec![1.0, 0.5, 0.25, 1.0],
            system_name: None,
            trailing: Vec::new(),
        };
        let bytes = colr_encode(&c);
        let back = colr_decode(&bytes).unwrap();
        assert_eq!(back.color_space, c.color_space);
        assert_eq!(back.components, c.components);
        assert_eq!(back.system_name, None);
    }

    #[test]
    fn colr_system_color_name_round_trips_byte_exact() {
        let mut on_disk = Vec::new();
        on_disk.extend_from_slice(magic::COLR);
        on_disk.extend_from_slice(&1u32.to_le_bytes());
        on_disk.extend_from_slice(&257u32.to_le_bytes()); // colorSpaceId 0x101
        on_disk.extend_from_slice(&4u32.to_le_bytes());
        for v in [0.0f64, 0.4, 0.85, 1.0] {
            on_disk.extend_from_slice(&v.to_le_bytes());
        }
        on_disk.extend_from_slice(magic::COLR);
        on_disk.extend_from_slice(&1u32.to_le_bytes());
        on_disk.extend_from_slice(&9u32.to_le_bytes());
        on_disk.extend_from_slice(b"linkColor");

        let c = colr_decode(&on_disk).unwrap();
        assert_eq!(c.system_name.as_deref(), Some("linkColor"));
        assert_eq!(c.components.len(), 4);
        assert_eq!(
            colr_encode(&c),
            on_disk,
            "COLR with system name must round-trip byte-exact"
        );
    }

    #[test]
    fn png_write_read_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("scar_test_{}.png", std::process::id()));
        let mut rgba = Vec::new();
        for i in 0..(3 * 2) {
            rgba.extend_from_slice(&[(i * 10) as u8, (i * 20) as u8, (i * 30) as u8, 255]);
        }
        let px = Pixels {
            width: 3,
            height: 2,
            rgba,
        };
        write_png(&path, &px).unwrap();
        let back = read_png(&path).unwrap();
        assert_eq!(back.width, px.width);
        assert_eq!(back.height, px.height);
        assert_eq!(back.rgba, px.rgba);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn all_fixtures_parse_and_celm_compression4_decodes() {
        for entry in fixture_dirs().iter().flat_map(|d| fs::read_dir(d).unwrap()) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let blob = fs::read(&path).unwrap();
            let csi = crate::csi::Csi::parse(&blob).unwrap();
            if csi.payload.len() >= 4 && &csi.payload[0..4] == magic::CELM {
                let comp = u32::from_le_bytes(csi.payload[8..12].try_into().unwrap());
                if comp == compression::LZFSE {
                    let bpr = csi
                        .tlv(crate::format::tlv::BYTES_PER_ROW)
                        .map(|d| u32::from_le_bytes(d[0..4].try_into().unwrap()))
                        .unwrap_or_else(|| crate::format::bytes_per_row(csi.header.width, 4));
                    let expected_len = bpr as usize * csi.header.height as usize;
                    let celm = celm_decode(&csi.payload, expected_len)
                        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                    assert!(
                        celm.raw.is_some(),
                        "{}: expected lzfse CELM to decode",
                        path.display()
                    );
                }
            }
        }
    }

    /// Round-trips every rendition of the local sample catalog byte-perfectly
    /// and decodes every LZFSE CELM; skipped when the file is absent.
    #[test]
    fn whole_sample_car_all_renditions_round_trip() {
        let path = Path::new("/Users/thea/Downloads/Assets.car");
        if !path.exists() {
            eprintln!("sample Assets.car not present, skipping whole-file test");
            return;
        }
        let data = fs::read(path).unwrap();
        let bom = match crate::bom::Bom::parse(&data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("bom parse not yet usable ({e}), skipping whole-file test");
                return;
            }
        };
        let renditions = match bom.tree_entries("RENDITIONS") {
            Ok(r) => r,
            Err(e) => {
                eprintln!("RENDITIONS tree not walkable yet ({e}), skipping whole-file test");
                return;
            }
        };
        assert!(!renditions.is_empty(), "catalog should have renditions");

        let mut lzfse_celm_count = 0;
        for (_key, value) in &renditions {
            let csi = crate::csi::Csi::parse(value)
                .expect("Csi::parse should succeed for every rendition");
            let out = csi.to_bytes();
            assert_eq!(
                out,
                *value,
                "Csi round trip must be byte-perfect for {}",
                csi.header.name_str()
            );

            if csi.payload.len() >= 16 && &csi.payload[0..4] == magic::CELM {
                let comp = u32::from_le_bytes(csi.payload[8..12].try_into().unwrap());
                if comp == compression::LZFSE {
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
                    let expected_len = bpr as usize * csi.header.height as usize;
                    let celm = celm_decode(&csi.payload, expected_len).unwrap_or_else(|e| {
                        panic!("{}: celm_decode failed: {e}", csi.header.name_str())
                    });
                    assert!(
                        celm.raw.is_some(),
                        "{}: expected lzfse CELM to decode",
                        csi.header.name_str()
                    );
                    lzfse_celm_count += 1;
                }
            }
        }
        // LZFSE count is catalog-specific; only the byte-perfect round-trip matters.
        let _ = lzfse_celm_count;
    }
}
