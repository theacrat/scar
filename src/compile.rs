//! `scar compile`: build a .car from a decompiled directory (inverse of
//! `decompile.rs`). See docs/FORMAT.md for the byte layouts.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::bom::BomWriter;
use crate::codec::{self, Pixels};
use crate::csi::{Csi, CsiHeader, Tlv};
use crate::format::{self, compression, magic, pixel_format, tlv};
use crate::manifest::{
    CarInfo, Composition, Content, ExtendedMetadata, Manifest, Metrics, Rendition,
};

pub fn compile(dir: &Path, out: &Path) -> Result<()> {
    let manifest = Manifest::load(&dir.join(crate::manifest::MANIFEST_NAME))
        .with_context(|| format!("loading manifest.json from {}", dir.display()))?;

    let key_ids: Vec<u32> = manifest
        .car
        .key_format
        .iter()
        .map(|name| {
            format::attribute_id(name)
                .ok_or_else(|| anyhow!("unknown attribute name in key_format: {name}"))
        })
        .collect::<Result<_>>()?;

    let mut w = BomWriter::new();

    let header_bytes = build_car_header(&manifest.car, manifest.renditions.len() as u32)?;
    let header_block = w.add_block(header_bytes);
    w.set_var("CARHEADER", header_block);

    let meta_bytes = build_extended_metadata(&manifest.car.metadata);
    let meta_block = w.add_block(meta_bytes);
    w.set_var("EXTENDED_METADATA", meta_block);

    let keyformat_bytes = build_keyformat(&key_ids);
    let keyformat_block = w.add_block(keyformat_bytes);
    w.set_var("KEYFORMAT", keyformat_block);

    // APPEARANCEKEYS/LOCALIZATIONKEYS vars are omitted entirely when empty, not written as empty trees.
    if !manifest.appearances.is_empty() {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = manifest
            .appearances
            .iter()
            .map(|(name, val)| (name.as_bytes().to_vec(), val.to_le_bytes().to_vec()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        w.add_tree("APPEARANCEKEYS", &entries, 4096);
    }

    if !manifest.localizations.is_empty() {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = manifest
            .localizations
            .iter()
            .map(|(name, val)| (name.as_bytes().to_vec(), val.to_le_bytes().to_vec()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        w.add_tree("LOCALIZATIONKEYS", &entries, 4096);
    }

    let mut facet_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(manifest.facets.len());
    for facet in &manifest.facets {
        let (hx, hy) = facet.hotspot.unwrap_or((0, 0));
        let mut pairs: Vec<(u16, u16)> = Vec::with_capacity(facet.attributes.len());
        for (name, val) in &facet.attributes {
            let id = format::attribute_id(name)
                .with_context(|| format!("facet {}: unknown attribute name {name}", facet.name))?;
            pairs.push((id as u16, *val));
        }
        pairs.sort_by_key(|(id, _)| *id);

        let mut value = Vec::with_capacity(6 + pairs.len() * 4);
        value.extend_from_slice(&hx.to_le_bytes());
        value.extend_from_slice(&hy.to_le_bytes());
        value.extend_from_slice(&(pairs.len() as u16).to_le_bytes());
        for (id, v) in pairs {
            value.extend_from_slice(&id.to_le_bytes());
            value.extend_from_slice(&v.to_le_bytes());
        }
        facet_entries.push((facet.name.as_bytes().to_vec(), value));
    }
    facet_entries.sort_by(|a, b| a.0.cmp(&b.0));
    w.add_tree("FACETKEYS", &facet_entries, 4096);

    let mut bitmap_entries: Vec<(u32, Vec<u8>)> = Vec::with_capacity(manifest.bitmap_keys.len());
    for (key, b64) in &manifest.bitmap_keys {
        let data = B64
            .decode(b64)
            .with_context(|| format!("decoding bitmap_keys[{key}]"))?;
        bitmap_entries.push((*key, data));
    }
    // No sort needed: BTreeMap iteration is already ascending by key.
    w.add_tree_inline_keys("BITMAPKEYS", &bitmap_entries, 1024);

    let atlas_overrides = apply_link_edits(dir, &manifest)?;
    let mut rendition_entries: Vec<(Vec<u8>, Vec<u8>)> =
        Vec::with_capacity(manifest.renditions.len());
    for (i, r) in manifest.renditions.iter().enumerate() {
        let key_bytes = build_key_bytes(r, &manifest.car.key_format);
        let value = build_csi(dir, r, atlas_overrides.get(&i))
            .with_context(|| format!("building rendition {:?} ({})", r.name, r.layout))?;
        rendition_entries.push((key_bytes, value));
    }
    rendition_entries.sort_by(|a, b| a.0.cmp(&b.0));
    w.add_tree("RENDITIONS", &rendition_entries, 4096);

    let data = w.finish();
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(out, &data).with_context(|| format!("writing {}", out.display()))?;

    println!("Compiled {} -> {}", dir.display(), out.display());
    println!("  renditions:  {}", manifest.renditions.len());
    println!("  facets:      {}", manifest.facets.len());
    println!("  appearances: {}", manifest.appearances.len());
    println!("  bitmap keys: {}", manifest.bitmap_keys.len());

    Ok(())
}

// CARHEADER / EXTENDED_METADATA / KEYFORMAT builders: docs/FORMAT.md §2-4.

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        bail!("hex string has odd length: {s:?}");
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks(2) {
        let text = std::str::from_utf8(chunk).context("hex string is not valid UTF-8")?;
        let byte =
            u8::from_str_radix(text, 16).with_context(|| format!("invalid hex byte {text:?}"))?;
        out.push(byte);
    }
    Ok(out)
}

fn write_cstr_field(field: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(field.len());
    field[..n].copy_from_slice(&bytes[..n]);
}

fn build_car_header(car: &CarInfo, rendition_count: u32) -> Result<Vec<u8>> {
    let mut out = vec![0u8; 436];
    out[0..4].copy_from_slice(magic::CAR_HEADER);
    out[4..8].copy_from_slice(&car.coreui_version.to_le_bytes());
    out[8..12].copy_from_slice(&car.storage_version.to_le_bytes());
    out[12..16].copy_from_slice(&car.storage_timestamp.to_le_bytes());
    out[16..20].copy_from_slice(&rendition_count.to_le_bytes());
    write_cstr_field(&mut out[20..148], &car.main_version_string);
    write_cstr_field(&mut out[148..404], &car.version_string);
    let uuid_bytes = hex_decode(&car.uuid).context("parsing car.uuid")?;
    if uuid_bytes.len() != 16 {
        bail!(
            "car.uuid must decode to 16 bytes (32 hex chars), got {}",
            uuid_bytes.len()
        );
    }
    out[404..420].copy_from_slice(&uuid_bytes);
    out[420..424].copy_from_slice(&car.associated_checksum.to_le_bytes());
    out[424..428].copy_from_slice(&car.schema_version.to_le_bytes());
    out[428..432].copy_from_slice(&car.color_space_id.to_le_bytes());
    out[432..436].copy_from_slice(&car.key_semantics.to_le_bytes());
    Ok(out)
}

fn build_extended_metadata(meta: &Option<ExtendedMetadata>) -> Vec<u8> {
    let mut out = vec![0u8; 1028];
    out[0..4].copy_from_slice(magic::EXTENDED_METADATA);
    let empty = ExtendedMetadata::default();
    let m = meta.as_ref().unwrap_or(&empty);
    write_cstr_field(&mut out[4..260], &m.thinning_arguments);
    write_cstr_field(&mut out[260..516], &m.deployment_platform_version);
    write_cstr_field(&mut out[516..772], &m.deployment_platform);
    write_cstr_field(&mut out[772..1028], &m.authoring_tool);
    out
}

fn build_keyformat(key_ids: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + key_ids.len() * 4);
    out.extend_from_slice(magic::KEY_FORMAT);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(key_ids.len() as u32).to_le_bytes());
    for id in key_ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn build_key_bytes(r: &Rendition, key_format: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key_format.len() * 2);
    for name in key_format {
        let val = r.key.get(name).copied().unwrap_or(0);
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

fn encode_slices(slices: &[[u32; 4]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + slices.len() * 16);
    out.extend_from_slice(&(slices.len() as u32).to_le_bytes());
    for s in slices {
        for v in s {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

fn encode_metrics(m: &Metrics) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    out.extend_from_slice(&1u32.to_le_bytes());
    for v in m.edge_insets {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&m.image_size.0.to_le_bytes());
    out.extend_from_slice(&m.image_size.1.to_le_bytes());
    out
}

fn encode_composition(c: &Composition) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&c.blend_mode.to_le_bytes());
    out.extend_from_slice(&c.opacity.to_le_bytes());
    out
}

fn parse_tag_hex(s: &str) -> Result<u32> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(stripped, 16).with_context(|| format!("invalid TLV tag {s:?}"))
}

fn bpp_for_pixel_format(pf: u32) -> Option<u32> {
    match pf {
        x if x == pixel_format::ARGB => Some(4),
        x if x == pixel_format::GA8 => Some(2),
        // Must match decompile's bytesPerRow (WBGR is RGBA16F, 8 B/px).
        x if x == crate::widegamut::WBGR => Some(8),
        x if x == crate::widegamut::GA16 => Some(4),
        _ => None,
    }
}

/// Returns (payload bytes, is-CELM-bitmap — which controls TLV 0x3EF).
/// `override_px` (atlas with link edits pasted in) forces a re-encode.
fn build_payload(
    dir: &Path,
    r: &Rendition,
    override_px: Option<&Pixels>,
) -> Result<(Vec<u8>, bool)> {
    match &r.content {
        Content::Image {
            file,
            compression: comp_name,
            original,
            edit_hash,
        } => {
            // Unchanged PNG with a kept original: write it back verbatim (re-encoding is lossy through un/re-premultiply).
            if override_px.is_none() {
                if let (Some(orig), Some(orig_hash)) = (original, edit_hash) {
                    if let Ok(png_bytes) = std::fs::read(dir.join(file)) {
                        if hash_bytes(&png_bytes) == *orig_hash {
                            let data = std::fs::read(dir.join(orig))
                                .with_context(|| format!("reading {orig}"))?;
                            return Ok((data, true));
                        }
                    }
                }
            }
            let read_px;
            let px = match override_px {
                Some(p) => p,
                None => {
                    read_px = codec::read_png(&dir.join(file))
                        .with_context(|| format!("reading {file}"))?;
                    &read_px
                }
            };
            let pf = pixel_format::from_name(&r.pixel_format)
                .ok_or_else(|| anyhow!("unknown pixel_format {:?}", r.pixel_format))?;
            let bpp = bpp_for_pixel_format(pf).ok_or_else(|| {
                anyhow!(
                    "pixel_format {:?} has no known bytes-per-pixel for Image content",
                    r.pixel_format
                )
            })?;
            let bpr = format::bytes_per_row(r.width, bpp);
            let raw = codec::rgba_to_raw(px, bpr, pf)
                .with_context(|| format!("converting {file} to raw pixels"))?;
            let comp = compression::from_name(comp_name)
                .ok_or_else(|| anyhow!("unknown compression {comp_name:?}"))?;
            let payload = codec::celm_encode(&raw, bpr, comp)
                .with_context(|| format!("encoding CELM payload for {file}"))?;
            Ok((payload, true))
        }
        Content::Data { file, lzfse } => {
            let data = std::fs::read(dir.join(file)).with_context(|| format!("reading {file}"))?;
            Ok((codec::rawd_encode(&data, *lzfse), false))
        }
        Content::Link { .. } => Ok((Vec::new(), false)),
        Content::Multisize { sizes } => {
            let entries: Vec<codec::MsisEntry> = sizes
                .iter()
                .map(|s| codec::MsisEntry {
                    width: s.width,
                    height: s.height,
                    index: s.index,
                })
                .collect();
            Ok((codec::msis_encode(&entries), false))
        }
        Content::Color {
            color_space,
            components,
            system_color,
            extra,
        } => {
            let trailing = if extra.is_empty() {
                Vec::new()
            } else {
                B64.decode(extra).context("decoding Color extra")?
            };
            Ok((
                codec::colr_encode(&codec::Color {
                    color_space: *color_space,
                    components: components.clone(),
                    system_name: system_color.clone(),
                    trailing,
                }),
                false,
            ))
        }
        Content::Gradient {
            gradient_type,
            reserved,
            start,
            end,
            stops,
        } => {
            let argg = crate::argg::Argg {
                gradient_type: *gradient_type,
                reserved: *reserved,
                start: (start[0], start[1]),
                end: (end[0], end[1]),
                stops: stops
                    .iter()
                    .map(|s| {
                        // On-disk name is NUL-terminated; nameLen == strlen+1.
                        let mut name = s.color_name.clone().into_bytes();
                        name.push(0);
                        crate::argg::GradientStop {
                            location: s.location,
                            name,
                        }
                    })
                    .collect(),
                trailing: Vec::new(),
            };
            Ok((crate::argg::encode(&argg), false))
        }
        Content::RawPayload {
            file,
            preview,
            edit_hash,
            ..
        } => {
            // Pasted link edits force a re-encode regardless of the atlas's own preview hash.
            if let Some(px) = override_px {
                return Ok((reencode_edited_px(r, px)?, true));
            }
            let data = std::fs::read(dir.join(file)).with_context(|| format!("reading {file}"))?;
            // Unedited preview (hash unchanged) → original payload verbatim, keeping the round-trip byte-exact.
            if let (Some(png), Some(orig_hash)) = (preview, edit_hash) {
                let png_path = dir.join(png);
                if let Ok(png_bytes) = std::fs::read(&png_path) {
                    if hash_bytes(&png_bytes) != *orig_hash {
                        let payload = reencode_edited(dir, r, png)?;
                        return Ok((payload, true));
                    }
                }
            }
            let is_celm = data.len() >= 4 && &data[0..4] == magic::CELM;
            Ok((data, is_celm))
        }
    }
}

/// Stable content hash matching `decompile::hash_bytes` (fixed-key SipHash).
fn hash_bytes(data: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn distinct_colors_at_most(rgba: &[u8], max: usize) -> bool {
    let mut seen = std::collections::HashSet::with_capacity(max + 1);
    for px in rgba.chunks_exact(4) {
        seen.insert([px[0], px[1], px[2], px[3]]);
        if seen.len() > max {
            return false;
        }
    }
    true
}

/// Re-encode an edited preview PNG into a native CELM payload, preferring the
/// rendition's original codec; falls back to a plain LZFSE bitmap.
fn reencode_edited(dir: &Path, r: &Rendition, png: &str) -> Result<Vec<u8>> {
    let px =
        codec::read_png(&dir.join(png)).with_context(|| format!("reading edited preview {png}"))?;
    reencode_edited_px(r, &px)
}

fn reencode_edited_px(r: &Rendition, px: &Pixels) -> Result<Vec<u8>> {
    let pf = pixel_format::from_name(&r.pixel_format)
        .ok_or_else(|| anyhow!("unknown pixel_format {:?}", r.pixel_format))?;
    let kind = match &r.content {
        Content::RawPayload { kind, .. } => kind.as_str(),
        _ => "",
    };

    // <=256 distinct colors: exact palette payload; richer edits fall through (median-cut would be lossy).
    if kind == "celm-deepmap2" && pf == pixel_format::ARGB && distinct_colors_at_most(&px.rgba, 256)
    {
        if let Some(payload) = crate::deepmap_encode::encode_palette(px)? {
            return Ok(payload);
        }
    }

    if kind == "celm-deepmap2" && (pf == pixel_format::ARGB || pf == pixel_format::GA8) {
        if let Some(payload) = crate::deepmap_encode::encode_default(px, pf)? {
            return Ok(payload);
        }
    }

    // WBGR: keep the RGBA16F container so the rendition's pixel format is unchanged.
    if pf == crate::widegamut::WBGR {
        let bpr = format::bytes_per_row(px.width, 8);
        let raw = crate::widegamut::rgba_to_wbgr_raw(px, bpr);
        return codec::celm_encode(&raw, bpr, compression::LZFSE);
    }

    let bpp = match pf {
        x if x == pixel_format::ARGB => 4u32,
        x if x == pixel_format::GA8 => 2u32,
        _ => bail!(
            "cannot re-encode edited rendition with pixel format {:?}",
            r.pixel_format
        ),
    };
    let bpr = format::bytes_per_row(px.width, bpp);
    let raw = codec::rgba_to_raw(px, bpr, pf)?;

    // GA8 only: CoreUI garbles ARGB RLE streams, so those fall through to the LZFSE bitmap.
    if kind == "celm-rle" && pf == pixel_format::GA8 {
        if let Some(body) = crate::rle::encode(&raw, px.width, px.height, bpr, pf) {
            let mut payload = Vec::with_capacity(16 + body.len());
            payload.extend_from_slice(magic::CELM);
            payload.extend_from_slice(&0u32.to_le_bytes()); // flags
            payload.extend_from_slice(&compression::RLE.to_le_bytes());
            payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
            payload.extend_from_slice(&body);
            return Ok(payload);
        }
    }

    codec::celm_encode(&raw, bpr, compression::LZFSE)
}

/// Promote GA8 -> ARGB when an edit introduced chroma (R!=G or G!=B), which GA8 re-encoding would silently discard.
/// Never triggers on a plain round-trip: decoded GA8 pixels are grayscale by construction.
fn promote_edited_ga8<'a>(
    dir: &Path,
    r: &'a Rendition,
    override_px: Option<&Pixels>,
) -> Result<Cow<'a, Rendition>> {
    if r.pixel_format != "GA8" {
        return Ok(Cow::Borrowed(r));
    }
    fn has_color(px: &Pixels) -> bool {
        px.rgba
            .chunks_exact(4)
            .any(|p| p[0] != p[1] || p[1] != p[2])
    }
    let colored = if let Some(px) = override_px {
        has_color(px)
    } else {
        match &r.content {
            Content::Image { file, .. } => codec::read_png(&dir.join(file))
                .map(|px| has_color(&px))
                .unwrap_or(false),
            Content::RawPayload {
                preview: Some(png),
                edit_hash: Some(orig_hash),
                ..
            } => {
                let path = dir.join(png);
                match std::fs::read(&path) {
                    Ok(bytes) if hash_bytes(&bytes) != *orig_hash => codec::read_png(&path)
                        .map(|px| has_color(&px))
                        .unwrap_or(false),
                    _ => false,
                }
            }
            _ => false,
        }
    };
    if !colored {
        return Ok(Cow::Borrowed(r));
    }
    println!(
        "note: rendition {:?} edited with color content; promoting GA8 -> ARGB",
        r.name
    );
    let mut promoted = r.clone();
    promoted.pixel_format = "ARGB".to_string();
    // A preserved 0x3ef TLV would still describe the 2-byte GA8 stride.
    promoted.extra_tlvs.remove("0x3ef");
    Ok(Cow::Owned(promoted))
}

/// Paste changed link (INLK) preview PNGs into their target atlas pixels,
/// one combined buffer per atlas, keyed by the atlas's rendition index.
fn apply_link_edits(dir: &Path, manifest: &Manifest) -> Result<HashMap<usize, Pixels>> {
    let key_format = &manifest.car.key_format;
    let full_key = |attrs: &std::collections::BTreeMap<String, u16>| -> Vec<u16> {
        key_format
            .iter()
            .map(|name| attrs.get(name).copied().unwrap_or(0))
            .collect()
    };
    let mut key_index: HashMap<Vec<u16>, usize> = HashMap::new();
    for (i, r) in manifest.renditions.iter().enumerate() {
        key_index.insert(full_key(&r.key), i);
    }

    // BTreeMap keeps overlapping pastes and notices deterministic.
    let mut pastes: BTreeMap<usize, Vec<(usize, [u32; 4], Pixels)>> = BTreeMap::new();
    for (i, r) in manifest.renditions.iter().enumerate() {
        let Content::Link {
            target,
            rect,
            preview: Some(png),
            edit_hash: Some(orig_hash),
            ..
        } = &r.content
        else {
            continue;
        };
        let Ok(png_bytes) = std::fs::read(dir.join(png)) else {
            continue; // missing preview: treat as unedited
        };
        if hash_bytes(&png_bytes) == *orig_hash {
            continue;
        }
        let px = codec::read_png(&dir.join(png))
            .with_context(|| format!("reading edited link preview {png}"))?;
        if (px.width, px.height) != (rect[2], rect[3]) {
            bail!(
                "link {:?} ({png}): edited preview is {}x{} but the link's atlas rect is {}x{}; \
                 resize the image to match (scar does not resample)",
                r.name,
                px.width,
                px.height,
                rect[2],
                rect[3]
            );
        }
        let atlas_idx = *key_index.get(&full_key(target)).ok_or_else(|| {
            anyhow!(
                "link {:?} ({png}): target atlas rendition not found",
                r.name
            )
        })?;
        pastes.entry(atlas_idx).or_default().push((i, *rect, px));
    }

    let mut overrides = HashMap::new();
    for (atlas_idx, patches) in pastes {
        let atlas = &manifest.renditions[atlas_idx];
        // The base may itself carry user edits; pastes apply on top.
        let base = match &atlas.content {
            Content::Image { file, .. } => codec::read_png(&dir.join(file))
                .with_context(|| format!("reading atlas image {file}"))?,
            Content::RawPayload {
                preview: Some(png),
                edit_hash: Some(_),
                ..
            } => codec::read_png(&dir.join(png))
                .with_context(|| format!("reading atlas preview {png}"))?,
            _ => bail!(
                "cannot apply edited link preview(s): target atlas {:?} is not an editable bitmap",
                atlas.name
            ),
        };
        if (base.width, base.height) != (atlas.width, atlas.height) {
            bail!(
                "atlas {:?}: pixels are {}x{} but the rendition declares {}x{}",
                atlas.name,
                base.width,
                base.height,
                atlas.width,
                atlas.height
            );
        }
        let mut combined = base;
        for (link_idx, rect, patch) in &patches {
            paste_rect(&mut combined, *rect, patch).with_context(|| {
                format!(
                    "pasting edited link {:?} into atlas {:?}",
                    manifest.renditions[*link_idx].name, atlas.name
                )
            })?;
        }
        println!(
            "note: pasted {} edited link preview(s) into atlas {:?}",
            patches.len(),
            atlas.name
        );
        overrides.insert(atlas_idx, combined);
    }
    Ok(overrides)
}

/// Copy `src` into `dst` at `rect` (x, y, w, h); the rect's y origin is bottom-left (CoreGraphics), flipped here.
fn paste_rect(dst: &mut Pixels, rect: [u32; 4], src: &Pixels) -> Result<()> {
    let [x, y, w, h] = rect;
    let fits = x.checked_add(w).is_some_and(|right| right <= dst.width)
        && y.checked_add(h).is_some_and(|bottom| bottom <= dst.height);
    if !fits {
        bail!(
            "rect {rect:?} does not fit in a {}x{} atlas",
            dst.width,
            dst.height
        );
    }
    let y_top = dst.height - y - h;
    for row in 0..h {
        let src_start = (row * w * 4) as usize;
        let dst_start = (((y_top + row) * dst.width + x) * 4) as usize;
        let n = (w * 4) as usize;
        dst.rgba[dst_start..dst_start + n].copy_from_slice(&src.rgba[src_start..src_start + n]);
    }
    Ok(())
}

fn build_csi(dir: &Path, r: &Rendition, override_px: Option<&Pixels>) -> Result<Vec<u8>> {
    let r = promote_edited_ga8(dir, r, override_px)?;
    let r = r.as_ref();
    let pf = pixel_format::from_name(&r.pixel_format)
        .ok_or_else(|| anyhow!("unknown pixel_format {:?}", r.pixel_format))?;

    let (payload, is_celm_bitmap) = build_payload(dir, r, override_px)?;

    let mut header = CsiHeader {
        version: 1,
        flags: r.flags,
        width: r.width,
        height: r.height,
        scale_factor: r.scale,
        pixel_format: pf,
        color_space_id: r.color_space_id,
        mod_time: r.modified,
        layout: r.layout,
        name: Vec::new(),
        unknown_a: 1,
        unknown_b: 0,
    };
    header.set_name(&r.name);

    let mut tlvs: Vec<Tlv> = Vec::new();
    let mut emitted: BTreeSet<u32> = BTreeSet::new();

    if let Some(slices) = &r.slices {
        tlvs.push(Tlv {
            tag: tlv::SLICES,
            data: encode_slices(slices),
        });
        emitted.insert(tlv::SLICES);
    }
    if let Some(metrics) = &r.metrics {
        tlvs.push(Tlv {
            tag: tlv::METRICS,
            data: encode_metrics(metrics),
        });
        emitted.insert(tlv::METRICS);
    }
    if let Content::Link {
        target,
        rect,
        content_layout,
        ..
    } = &r.content
    {
        let mut pairs: Vec<(u16, u16)> = Vec::with_capacity(target.len());
        for (name, val) in target {
            let id = format::attribute_id(name)
                .ok_or_else(|| anyhow!("link target: unknown attribute name {name}"))?;
            pairs.push((id as u16, *val));
        }
        pairs.sort_by_key(|(id, _)| *id);
        pairs.push((0, 0));
        let inlk = codec::Inlk {
            flags: 0,
            x: rect[0],
            y: rect[1],
            width: rect[2],
            height: rect[3],
            content_layout: *content_layout,
            key_pairs: pairs,
        };
        tlvs.push(Tlv {
            tag: tlv::INTERNAL_LINK,
            data: codec::inlk_encode(&inlk),
        });
        emitted.insert(tlv::INTERNAL_LINK);
    }
    if let Some(comp) = &r.composition {
        tlvs.push(Tlv {
            tag: tlv::COMPOSITION,
            data: encode_composition(comp),
        });
        emitted.insert(tlv::COMPOSITION);
    }
    if let Some(bi) = r.bitmap_info {
        tlvs.push(Tlv {
            tag: tlv::BITMAP_INFO,
            data: bi.to_le_bytes().to_vec(),
        });
        emitted.insert(tlv::BITMAP_INFO);
    }
    if is_celm_bitmap {
        let bpr_bytes = if let Some(b64) = r.extra_tlvs.get("0x3ef") {
            B64.decode(b64).context("decoding extra_tlvs[\"0x3ef\"]")?
        } else {
            let bpp = bpp_for_pixel_format(pf).unwrap_or(4);
            format::bytes_per_row(r.width, bpp).to_le_bytes().to_vec()
        };
        tlvs.push(Tlv {
            tag: tlv::BYTES_PER_ROW,
            data: bpr_bytes,
        });
        emitted.insert(tlv::BYTES_PER_ROW);
    }

    // Remaining TLVs sorted by tag; extra_tlvs["0x3ef"] was already consumed above.
    let mut extras: Vec<(u32, &str)> = Vec::with_capacity(r.extra_tlvs.len());
    for (tag_hex, b64) in &r.extra_tlvs {
        let tag = parse_tag_hex(tag_hex)?;
        extras.push((tag, b64.as_str()));
    }
    extras.sort_by_key(|(tag, _)| *tag);
    for (tag, b64) in extras {
        if emitted.contains(&tag) {
            continue;
        }
        let data = B64
            .decode(b64)
            .with_context(|| format!("decoding extra_tlvs[\"0x{tag:x}\"]"))?;
        tlvs.push(Tlv { tag, data });
    }

    let csi = Csi {
        header,
        tlvs,
        payload,
    };
    Ok(csi.to_bytes())
}
