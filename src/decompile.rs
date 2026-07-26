//! `scar info` and `scar decompile`: read a .car, decode renditions, write
//! manifest.json + asset files. See docs/FORMAT.md for the byte layouts.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::bom::Bom;
use crate::codec::{self, Pixels};
use crate::csi::Csi;
use crate::format::{self, compression, magic, pixel_format, tlv};
use crate::manifest::{
    CarInfo, Composition, Content, ExtendedMetadata, Facet, Manifest, Metrics, MultisizeEntry,
    Rendition,
};

fn u32le(data: &[u8], off: usize) -> Result<u32> {
    let b: [u8; 4] = data
        .get(off..off + 4)
        .context("truncated (u32)")?
        .try_into()
        .unwrap();
    Ok(u32::from_le_bytes(b))
}

fn u16le(data: &[u8], off: usize) -> Result<u16> {
    let b: [u8; 2] = data
        .get(off..off + 2)
        .context("truncated (u16)")?
        .try_into()
        .unwrap();
    Ok(u16::from_le_bytes(b))
}

fn cstr(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

fn hex_lower(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Deterministic content hash (fixed-key SipHash), used to detect edited preview PNGs.
fn hash_bytes(data: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    format!("{:016x}", h.finish())
}

// CARHEADER / EXTENDED_METADATA / KEYFORMAT layouts: docs/FORMAT.md §2-4.

struct CarHeaderRaw {
    coreui_version: u32,
    storage_version: u32,
    storage_timestamp: u32,
    rendition_count: u32,
    main_version_string: String,
    version_string: String,
    uuid_hex: String,
    associated_checksum: u32,
    schema_version: u32,
    color_space_id: u32,
    key_semantics: u32,
}

fn parse_car_header(data: &[u8]) -> Result<CarHeaderRaw> {
    if data.len() < 436 {
        bail!("CARHEADER block too short: {} bytes", data.len());
    }
    if &data[0..4] != magic::CAR_HEADER {
        bail!("bad CARHEADER magic: {:?}", &data[0..4]);
    }
    Ok(CarHeaderRaw {
        coreui_version: u32le(data, 4)?,
        storage_version: u32le(data, 8)?,
        storage_timestamp: u32le(data, 12)?,
        rendition_count: u32le(data, 16)?,
        main_version_string: cstr(&data[20..20 + 128]),
        version_string: cstr(&data[148..148 + 256]),
        uuid_hex: hex_lower(&data[404..404 + 16]),
        associated_checksum: u32le(data, 420)?,
        schema_version: u32le(data, 424)?,
        color_space_id: u32le(data, 428)?,
        key_semantics: u32le(data, 432)?,
    })
}

fn parse_extended_metadata(data: &[u8]) -> Result<ExtendedMetadata> {
    if data.len() < 1028 {
        bail!("EXTENDED_METADATA block too short: {} bytes", data.len());
    }
    if &data[0..4] != magic::EXTENDED_METADATA {
        bail!("bad EXTENDED_METADATA magic: {:?}", &data[0..4]);
    }
    Ok(ExtendedMetadata {
        thinning_arguments: cstr(&data[4..4 + 256]),
        deployment_platform_version: cstr(&data[260..260 + 256]),
        deployment_platform: cstr(&data[516..516 + 256]),
        authoring_tool: cstr(&data[772..772 + 256]),
    })
}

/// Attribute ids, in on-disk KEYFORMAT order.
pub(crate) fn parse_keyformat(data: &[u8]) -> Result<Vec<u32>> {
    if data.len() < 12 {
        bail!("KEYFORMAT block too short: {} bytes", data.len());
    }
    if &data[0..4] != magic::KEY_FORMAT {
        bail!("bad KEYFORMAT magic: {:?}", &data[0..4]);
    }
    let num_tokens = u32le(data, 8)? as usize;
    let mut ids = Vec::with_capacity(num_tokens);
    let mut off = 12;
    for _ in 0..num_tokens {
        ids.push(u32le(data, off)?);
        off += 4;
    }
    Ok(ids)
}

/// Decode a tree key (numTokens * u16 LE) into values in keyformat order.
pub(crate) fn decode_key_vec(key_bytes: &[u8], num_tokens: usize) -> Result<Vec<u16>> {
    if key_bytes.len() != num_tokens * 2 {
        bail!(
            "rendition key length {} != {} tokens * 2",
            key_bytes.len(),
            num_tokens
        );
    }
    let mut out = Vec::with_capacity(num_tokens);
    for i in 0..num_tokens {
        out.push(u16le(key_bytes, i * 2)?);
    }
    Ok(out)
}

/// Key vector -> attribute-name map, omitting zero-valued attributes.
pub(crate) fn key_vec_to_map(key_ids: &[u32], values: &[u16]) -> BTreeMap<String, u16> {
    let mut m = BTreeMap::new();
    for (id, val) in key_ids.iter().zip(values.iter()) {
        if *val != 0 {
            m.insert(format::attribute_name(*id), *val);
        }
    }
    m
}

pub(crate) struct FacetRaw {
    pub(crate) name: String,
    pub(crate) hotspot: (u16, u16),
    pub(crate) attributes: BTreeMap<String, u16>,
}

pub(crate) fn parse_facet_value(name: &str, value: &[u8]) -> Result<FacetRaw> {
    if value.len() < 6 {
        bail!(
            "facet {name}: renditionkeytoken too short ({} bytes)",
            value.len()
        );
    }
    let hotspot_x = u16le(value, 0)?;
    let hotspot_y = u16le(value, 2)?;
    let n_pairs = u16le(value, 4)? as usize;
    let mut attributes = BTreeMap::new();
    let mut off = 6;
    for _ in 0..n_pairs {
        let attr = u16le(value, off)?;
        let val = u16le(value, off + 2)?;
        off += 4;
        attributes.insert(format::attribute_name(attr as u32), val);
    }
    Ok(FacetRaw {
        name: name.to_string(),
        hotspot: (hotspot_x, hotspot_y),
        attributes,
    })
}

/// Keep [A-Za-z0-9._-], replace everything else with '_'.
fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "_".to_string() } else { s }
}

/// Sanitized name with `ext`, replacing any existing differing extension.
fn filename_with_ext(name: &str, ext: &str) -> String {
    let sanitized = sanitize_name(name);
    if let Some(pos) = sanitized.rfind('.') {
        let stem = &sanitized[..pos];
        let existing_ext = &sanitized[pos + 1..];
        if existing_ext.eq_ignore_ascii_case(ext) {
            return sanitized;
        }
        if !stem.is_empty() {
            return format!("{stem}.{ext}");
        }
    }
    format!("{sanitized}.{ext}")
}

fn rel_path(dir: &str, idx: usize, name: &str, ext: &str) -> String {
    format!("{dir}/{idx:03}-{}", filename_with_ext(name, ext))
}

fn extract_slices(data: &[u8]) -> Option<Vec<[u32; 4]>> {
    if data.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    if data.len() != 4 + count * 16 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    let mut off = 4;
    for _ in 0..count {
        let a = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        let b = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?);
        let c = u32::from_le_bytes(data[off + 8..off + 12].try_into().ok()?);
        let d = u32::from_le_bytes(data[off + 12..off + 16].try_into().ok()?);
        out.push([a, b, c, d]);
        off += 16;
    }
    Some(out)
}

/// TLV 0x3EB: count(=1) + insets t/l/b/r + image w/h = 28 bytes; anything else falls back to extra_tlvs.
fn extract_metrics(data: &[u8]) -> Option<Metrics> {
    if data.len() != 28 {
        return None;
    }
    let count = u32::from_le_bytes(data[0..4].try_into().ok()?);
    if count != 1 {
        return None;
    }
    let top = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let left = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let bottom = u32::from_le_bytes(data[12..16].try_into().ok()?);
    let right = u32::from_le_bytes(data[16..20].try_into().ok()?);
    let iw = u32::from_le_bytes(data[20..24].try_into().ok()?);
    let ih = u32::from_le_bytes(data[24..28].try_into().ok()?);
    Some(Metrics {
        edge_insets: [top, left, bottom, right],
        image_size: (iw, ih),
    })
}

fn extract_composition(data: &[u8]) -> Option<Composition> {
    if data.len() != 8 {
        return None;
    }
    let blend_mode = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let opacity = f32::from_le_bytes(data[4..8].try_into().ok()?);
    Some(Composition {
        blend_mode,
        opacity,
    })
}

fn extract_bitmap_info(data: &[u8]) -> Option<u32> {
    if data.len() != 4 {
        return None;
    }
    Some(u32::from_le_bytes(data[0..4].try_into().ok()?))
}

fn bpp_for_pixel_format(pf: u32) -> Option<u32> {
    match pf {
        x if x == pixel_format::ARGB => Some(4),
        x if x == pixel_format::GA8 => Some(2),
        // Wide formats decode to previews only; round-trip stays RawPayload passthrough.
        x if x == crate::widegamut::WBGR => Some(8),
        x if x == crate::widegamut::GA16 => Some(4),
        _ => None,
    }
}

/// Raw CELM rows -> RGBA: standard ARGB/GA8 first, wide-gamut second. Previews only.
fn raw_to_rgba_any(raw: &[u8], width: u32, height: u32, bpr: u32, pf: u32) -> Option<Pixels> {
    if let Ok(px) = codec::raw_to_rgba(raw, width, height, bpr, pf) {
        return Some(px);
    }
    if crate::widegamut::is_wide_format(pf) {
        return crate::widegamut::to_rgba(raw, width, height, bpr, pf)
            .ok()
            .flatten();
    }
    None
}

fn bytes_per_row_for(csi: &Csi) -> u32 {
    let bpp = bpp_for_pixel_format(csi.header.pixel_format).unwrap_or(4);
    csi.tlv(tlv::BYTES_PER_ROW)
        .and_then(|d| {
            if d.len() == 4 {
                Some(u32::from_le_bytes(d[0..4].try_into().unwrap()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| format::bytes_per_row(csi.header.width, bpp))
}

/// Best-effort preview decode of a CELM bitmap; None on anything undecodable.
fn decode_atlas_bitmap(csi: &Csi) -> Option<Pixels> {
    let payload = &csi.payload;
    if payload.len() < 4 || &payload[0..4] != magic::CELM {
        return None;
    }
    let pixel_fmt = csi.header.pixel_format;
    bpp_for_pixel_format(pixel_fmt)?;
    let bpr = bytes_per_row_for(csi);
    let expected = bpr as usize * csi.header.height as usize;
    let celm = codec::celm_decode(payload, expected).ok()?;
    if let Some(raw) = celm.raw {
        return raw_to_rgba_any(&raw, csi.header.width, csi.header.height, bpr, pixel_fmt);
    }
    if celm.compression == compression::DEEPMAP2 {
        return crate::deepmap::decode(payload, csi.header.width, csi.header.height, pixel_fmt)
            .ok()
            .flatten();
    }
    if celm.compression == compression::RLE {
        // CELM stream body = payload after the 16-byte MLEC header.
        let field3 = u32::from_le_bytes(payload.get(12..16)?.try_into().ok()?) as usize;
        let stream = payload.get(16..16usize.checked_add(field3)?.min(payload.len()))?;
        let raw = crate::rle::decode(stream, csi.header.width, csi.header.height, bpr, pixel_fmt)
            .ok()
            .flatten()?;
        return codec::raw_to_rgba(&raw, csi.header.width, csi.header.height, bpr, pixel_fmt).ok();
    }
    None
}

fn crop_rect(px: &Pixels, x: u32, y: u32, w: u32, h: u32) -> Option<Pixels> {
    if w == 0 || h == 0 {
        return None;
    }
    if x.checked_add(w)? > px.width || y.checked_add(h)? > px.height {
        return None;
    }
    let mut out = vec![0u8; (w * h * 4) as usize];
    for row in 0..h {
        let src_start = (((y + row) * px.width + x) * 4) as usize;
        let src_end = src_start + (w * 4) as usize;
        let dst_start = (row * w * 4) as usize;
        out[dst_start..dst_start + (w * 4) as usize].copy_from_slice(&px.rgba[src_start..src_end]);
    }
    Some(Pixels {
        width: w,
        height: h,
        rgba: out,
    })
}

fn describe_kind(csi: &Csi) -> String {
    let payload = &csi.payload;
    if payload.len() >= 4 && &payload[0..4] == magic::CELM {
        if payload.len() >= 12 {
            let comp = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            format!("celm:{}", compression::name(comp))
        } else {
            "celm:?".to_string()
        }
    } else if payload.len() >= 4 && &payload[0..4] == magic::RAWD {
        "rawd".to_string()
    } else if payload.len() >= 4 && &payload[0..4] == magic::MSIS {
        "msis".to_string()
    } else if payload.len() >= 4 && &payload[0..4] == magic::COLR {
        "colr".to_string()
    } else if payload.is_empty() && csi.tlv(tlv::INTERNAL_LINK).is_some() {
        "link".to_string()
    } else if payload.is_empty() {
        "empty".to_string()
    } else {
        "unknown".to_string()
    }
}

pub fn info(car: &Path, list_renditions: bool) -> Result<()> {
    let data = fs::read(car).with_context(|| format!("reading {}", car.display()))?;
    let bom = Bom::parse(&data).context("parsing BOM container")?;

    let header_data = bom
        .var_block("CARHEADER")
        .context("missing CARHEADER var")?;
    let header = parse_car_header(header_data)?;

    let metadata = bom
        .var_block("EXTENDED_METADATA")
        .map(parse_extended_metadata)
        .transpose()
        .context("parsing EXTENDED_METADATA")?;

    let keyformat_data = bom
        .var_block("KEYFORMAT")
        .context("missing KEYFORMAT var")?;
    let key_ids = parse_keyformat(keyformat_data)?;
    let key_names: Vec<String> = key_ids
        .iter()
        .map(|id| format::attribute_name(*id))
        .collect();

    let appearances = bom.tree_entries("APPEARANCEKEYS").unwrap_or_default();
    let facets = bom.tree_entries("FACETKEYS").unwrap_or_default();
    let renditions = bom
        .tree_entries("RENDITIONS")
        .context("walking RENDITIONS tree")?;

    println!("{}", car.display());
    println!("  CoreUI version:    {}", header.coreui_version);
    println!("  Storage version:   {}", header.storage_version);
    println!("  Schema version:    {}", header.schema_version);
    println!("  Key semantics:     {}", header.key_semantics);
    println!("  Color space id:    {}", header.color_space_id);
    println!("  UUID:              {}", header.uuid_hex);
    println!("  Main version:      {}", header.main_version_string);
    println!("  Version string:    {}", header.version_string);
    if let Some(meta) = &metadata {
        println!(
            "  Platform:          {} {}",
            meta.deployment_platform, meta.deployment_platform_version
        );
        if !meta.authoring_tool.is_empty() {
            println!("  Authoring tool:    {}", meta.authoring_tool);
        }
        if !meta.thinning_arguments.is_empty() {
            println!("  Thinning args:     {}", meta.thinning_arguments);
        }
    }
    println!(
        "  Key format ({} tokens): {}",
        key_names.len(),
        key_names.join(", ")
    );
    println!(
        "  Renditions:        {} (header claims {})",
        renditions.len(),
        header.rendition_count
    );
    println!("  Facets:            {}", facets.len());
    println!("  Appearances:       {}", appearances.len());
    for (key, value) in &appearances {
        let name = String::from_utf8_lossy(key);
        let val = if value.len() >= 2 {
            u16::from_le_bytes([value[0], value[1]])
        } else {
            0
        };
        println!("    {name} = {val}");
    }

    let mut by_layout: BTreeMap<u16, usize> = BTreeMap::new();
    let mut by_compression: BTreeMap<u32, usize> = BTreeMap::new();
    let mut parsed: Vec<Csi> = Vec::with_capacity(renditions.len());
    for (_key, value) in &renditions {
        let csi = Csi::parse(value).context("parsing CSI blob")?;
        *by_layout.entry(csi.header.layout).or_default() += 1;
        if csi.payload.len() >= 12 && &csi.payload[0..4] == magic::CELM {
            let comp = u32::from_le_bytes(csi.payload[8..12].try_into().unwrap());
            *by_compression.entry(comp).or_default() += 1;
        }
        parsed.push(csi);
    }

    println!();
    println!("  By layout:");
    for (layout, count) in &by_layout {
        println!("    {layout:>5}: {count}");
    }
    println!("  By CELM compression:");
    for (comp, count) in &by_compression {
        println!("    {:>16}: {count}", compression::name(*comp));
    }

    if list_renditions {
        println!();
        for (i, ((key_bytes, _value), csi)) in renditions.iter().zip(parsed.iter()).enumerate() {
            let values = decode_key_vec(key_bytes, key_ids.len()).unwrap_or_default();
            let key_map = key_vec_to_map(&key_ids, &values);
            let key_str = key_map
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            let kind = describe_kind(csi);
            println!(
                "  {i:03} {:<40} layout={:<5} pixel={:<5} {}x{}@{} kind={:<14} key={key_str}",
                csi.header.name_str(),
                csi.header.layout,
                pixel_format::name(csi.header.pixel_format),
                csi.header.width,
                csi.header.height,
                csi.header.scale_factor as f64 / 100.0,
                kind,
            );
        }
    }

    Ok(())
}

#[derive(Default)]
struct Stats {
    images: usize,
    data: usize,
    links: usize,
    link_previews_ok: usize,
    multisize: usize,
    colors: usize,
    gradients: usize,
    raw_payloads: BTreeMap<String, usize>,
    dmp2_total: usize,
    dmp2_previews_ok: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DecompileOptions {
    /// Store every payload verbatim (no decoding).
    pub raw: bool,
    /// Skip all preview PNGs (atlas-link crops, deepmap2/rle/wide-gamut previews).
    /// Faster; the affected assets round-trip verbatim but their previews cannot be edited.
    pub skip_previews: bool,
}

pub fn decompile(car: &Path, out: &Path, raw: bool) -> Result<()> {
    decompile_with(
        car,
        out,
        &DecompileOptions {
            raw,
            ..Default::default()
        },
    )
}

pub fn decompile_with(car: &Path, out: &Path, opts: &DecompileOptions) -> Result<()> {
    let DecompileOptions { raw, skip_previews } = *opts;
    let data = fs::read(car).with_context(|| format!("reading {}", car.display()))?;
    let bom = Bom::parse(&data).context("parsing BOM container")?;

    fs::create_dir_all(out).with_context(|| format!("creating output dir {}", out.display()))?;
    for sub in ["renditions", "previews", "rawpayload", "data"] {
        fs::create_dir_all(out.join(sub))?;
    }

    let header_data = bom
        .var_block("CARHEADER")
        .context("missing CARHEADER var")?;
    let header = parse_car_header(header_data)?;

    let metadata = bom
        .var_block("EXTENDED_METADATA")
        .map(parse_extended_metadata)
        .transpose()
        .context("parsing EXTENDED_METADATA")?;

    let keyformat_data = bom
        .var_block("KEYFORMAT")
        .context("missing KEYFORMAT var")?;
    let key_ids = parse_keyformat(keyformat_data)?;
    let key_names: Vec<String> = key_ids
        .iter()
        .map(|id| format::attribute_name(*id))
        .collect();

    let mut appearances = BTreeMap::new();
    for (key, value) in bom.tree_entries("APPEARANCEKEYS").unwrap_or_default() {
        let name = String::from_utf8_lossy(&key).into_owned();
        let val = if value.len() >= 2 {
            u16::from_le_bytes([value[0], value[1]])
        } else {
            0
        };
        appearances.insert(name, val);
    }

    let mut localizations = BTreeMap::new();
    for (key, value) in bom.tree_entries("LOCALIZATIONKEYS").unwrap_or_default() {
        let name = String::from_utf8_lossy(&key).into_owned();
        let val = if value.len() >= 2 {
            u16::from_le_bytes([value[0], value[1]])
        } else {
            0
        };
        localizations.insert(name, val);
    }

    let mut facets = Vec::new();
    for (key, value) in bom.tree_entries("FACETKEYS").unwrap_or_default() {
        let name = String::from_utf8_lossy(&key).into_owned();
        match parse_facet_value(&name, &value) {
            Ok(f) => facets.push(Facet {
                name: f.name,
                hotspot: Some(f.hotspot),
                attributes: f.attributes,
            }),
            Err(e) => eprintln!("warning: skipping unparseable facet {name}: {e}"),
        }
    }

    // BITMAPKEYS values are opaque; preserved as base64.
    let mut bitmap_keys = BTreeMap::new();
    if let Ok(entries) = bom.tree_entries_inline_keys("BITMAPKEYS") {
        for (key, value) in entries {
            bitmap_keys.insert(key, B64.encode(&value));
        }
    }

    // Key -> index lookup lets link targets (packed atlases) be resolved for previews.
    let rendition_entries = bom
        .tree_entries("RENDITIONS")
        .context("walking RENDITIONS tree")?;
    let mut csis: Vec<Csi> = Vec::with_capacity(rendition_entries.len());
    let mut key_vecs: Vec<Vec<u16>> = Vec::with_capacity(rendition_entries.len());
    for (key_bytes, value_bytes) in &rendition_entries {
        let csi = Csi::parse(value_bytes)
            .with_context(|| format!("parsing CSI blob for key {}", hex_lower(key_bytes)))?;
        let values = decode_key_vec(key_bytes, key_ids.len())?;
        key_vecs.push(values);
        csis.push(csi);
    }
    let mut key_index: HashMap<Vec<u16>, usize> = HashMap::new();
    for (i, kv) in key_vecs.iter().enumerate() {
        key_index.insert(kv.clone(), i);
    }

    let mut stats = Stats::default();
    let mut out_renditions: Vec<Rendition> = Vec::with_capacity(csis.len());
    let mut atlas_cache: HashMap<usize, Option<Rc<Pixels>>> = HashMap::new();

    for (i, csi) in csis.iter().enumerate() {
        let name = csi.header.name_str();
        let width = csi.header.width;
        let height = csi.header.height;
        let pixel_fmt = csi.header.pixel_format;
        let bpp = bpp_for_pixel_format(pixel_fmt).unwrap_or(4);
        let derived_bpr = format::bytes_per_row(width, bpp);
        let bpr = bytes_per_row_for(csi);

        let mut slices = None;
        let mut metrics = None;
        let mut composition = None;
        let mut bitmap_info = None;
        let mut extra_tlvs: BTreeMap<String, String> = BTreeMap::new();
        for t in &csi.tlvs {
            match t.tag {
                tlv::SLICES => match extract_slices(&t.data) {
                    Some(s) => slices = Some(s),
                    None => {
                        extra_tlvs.insert(format!("0x{:x}", t.tag), B64.encode(&t.data));
                    }
                },
                tlv::METRICS => match extract_metrics(&t.data) {
                    Some(m) => metrics = Some(m),
                    None => {
                        extra_tlvs.insert(format!("0x{:x}", t.tag), B64.encode(&t.data));
                    }
                },
                tlv::COMPOSITION => match extract_composition(&t.data) {
                    Some(c) => composition = Some(c),
                    None => {
                        extra_tlvs.insert(format!("0x{:x}", t.tag), B64.encode(&t.data));
                    }
                },
                tlv::BITMAP_INFO => match extract_bitmap_info(&t.data) {
                    Some(b) => bitmap_info = Some(b),
                    None => {
                        extra_tlvs.insert(format!("0x{:x}", t.tag), B64.encode(&t.data));
                    }
                },
                tlv::BYTES_PER_ROW => {
                    let val = if t.data.len() == 4 {
                        u32::from_le_bytes(t.data[0..4].try_into().unwrap())
                    } else {
                        u32::MAX
                    };
                    if val != derived_bpr {
                        extra_tlvs.insert(format!("0x{:x}", t.tag), B64.encode(&t.data));
                    }
                }
                tlv::INTERNAL_LINK => { /* materialized as Content::Link below */ }
                other => {
                    extra_tlvs.insert(format!("0x{other:x}"), B64.encode(&t.data));
                }
            }
        }

        let payload = &csi.payload;
        let is_celm = payload.len() >= 4 && &payload[0..4] == magic::CELM;
        let is_rawd = payload.len() >= 4 && &payload[0..4] == magic::RAWD;
        let is_msis = payload.len() >= 4 && &payload[0..4] == magic::MSIS;
        let is_colr = payload.len() >= 4 && &payload[0..4] == magic::COLR;
        let link_tlv = csi.tlv(tlv::INTERNAL_LINK);

        let content = if is_msis {
            let entries = codec::msis_decode(payload)
                .with_context(|| format!("decoding MSIS payload for rendition {i} ({name})"))?;
            stats.multisize += 1;
            Content::Multisize {
                sizes: entries
                    .into_iter()
                    .map(|e| MultisizeEntry {
                        width: e.width,
                        height: e.height,
                        index: e.index,
                    })
                    .collect(),
            }
        } else if payload.is_empty() && link_tlv.is_some() {
            let link = codec::inlk_decode(link_tlv.unwrap())
                .with_context(|| format!("decoding INLK tlv for rendition {i} ({name})"))?;
            let target: BTreeMap<String, u16> = link
                .key_pairs
                .iter()
                .take_while(|(attr, _)| *attr != 0)
                .map(|(attr, val)| (format::attribute_name(*attr as u32), *val))
                .collect();

            // Full target key: keyformat attrs default 0, overridden by the link's pairs.
            let mut target_full = vec![0u16; key_ids.len()];
            for (pos, id) in key_ids.iter().enumerate() {
                let attr_name = format::attribute_name(*id);
                if let Some(v) = target.get(&attr_name) {
                    target_full[pos] = *v;
                }
            }
            // edit_hash lets compile detect an edited preview and paste it back into the atlas.
            let mut edit_hash = None;
            let mut preview = None;
            if !skip_previews {
                if let Some(&target_idx) = key_index.get(&target_full) {
                    // Many links share one atlas; decode each at most once.
                    let atlas = atlas_cache
                        .entry(target_idx)
                        .or_insert_with(|| decode_atlas_bitmap(&csis[target_idx]).map(Rc::new))
                        .clone();
                    preview = atlas.and_then(|px| {
                        // INLK rect y is bottom-up (docs/FORMAT.md §6.4); flip here, manifest keeps the stored rect.
                        let y_top = px.height.checked_sub(link.y)?.checked_sub(link.height)?;
                        let cropped = crop_rect(&px, link.x, y_top, link.width, link.height)?;
                        let file = rel_path("previews", i, &name, "png");
                        codec::write_png(&out.join(&file), &cropped).ok()?;
                        if let Ok(bytes) = fs::read(out.join(&file)) {
                            edit_hash = Some(hash_bytes(&bytes));
                        }
                        Some(file)
                    });
                }
                if preview.is_some() {
                    stats.link_previews_ok += 1;
                }
            }
            stats.links += 1;
            Content::Link {
                target,
                rect: [link.x, link.y, link.width, link.height],
                content_layout: link.content_layout,
                preview,
                edit_hash,
            }
        } else if is_celm {
            let comp_from_payload = if payload.len() >= 12 {
                u32::from_le_bytes(payload[8..12].try_into().unwrap())
            } else {
                0
            };
            if raw {
                let file = write_raw_payload(out, i, &name, payload)?;
                *stats
                    .raw_payloads
                    .entry(format!("celm-{}", compression::name(comp_from_payload)))
                    .or_default() += 1;
                Content::RawPayload {
                    file,
                    kind: format!("celm-{}", compression::name(comp_from_payload)),
                    preview: None,
                    edit_hash: None,
                }
            } else {
                let expected_len = bpr as usize * height as usize;
                // A bad declared stride must not abort decompile; fall back to lossless passthrough.
                let celm = codec::celm_decode(payload, expected_len).unwrap_or_else(|_| {
                    let comp = if payload.len() >= 12 {
                        u32::from_le_bytes(payload[8..12].try_into().unwrap())
                    } else {
                        0
                    };
                    codec::Celm {
                        flags: 0,
                        compression: comp,
                        raw: None,
                    }
                });
                // Stride can contradict the format name (layout 1008: BGRA tag, 8 B/px); on mismatch pass through verbatim or re-encode corrupts (docs/FORMAT.md §5.2).
                let stride_consistent = bpp_for_pixel_format(pixel_fmt)
                    .map(|bpp| bpr == format::bytes_per_row(width, bpp))
                    .unwrap_or(false);
                let decoded_px = if stride_consistent {
                    celm.raw.as_ref().and_then(|raw_bytes| {
                        codec::raw_to_rgba(raw_bytes, width, height, bpr, pixel_fmt).ok()
                    })
                } else {
                    None
                };
                match decoded_px {
                    Some(px) => {
                        let file = rel_path("renditions", i, &name, "png");
                        codec::write_png(&out.join(&file), &px)?;
                        stats.images += 1;
                        // Semi-transparent pixels don't survive the un/re-premultiply round trip; keep the original payload unless the PNG is edited.
                        let has_semitransparent =
                            px.rgba.chunks_exact(4).any(|p| p[3] != 0 && p[3] != 255);
                        let (original, edit_hash) = if has_semitransparent {
                            let bin = write_raw_payload(out, i, &name, payload)?;
                            let hash = fs::read(out.join(&file)).ok().map(|b| hash_bytes(&b));
                            (Some(bin), hash)
                        } else {
                            (None, None)
                        };
                        Content::Image {
                            file,
                            compression: compression::name(celm.compression),
                            original,
                            edit_hash,
                        }
                    }
                    None if celm.raw.is_some() => {
                        // Pixel format not re-encodable (WBGR/GA16): verbatim passthrough plus a preview when possible.
                        let file = write_raw_payload(out, i, &name, payload)?;
                        let kind = format!(
                            "celm-{}-{}",
                            compression::name(celm.compression),
                            pixel_format::name(pixel_fmt)
                        );
                        // WBGR has a re-encoder, so its preview is editable; GA16 does not.
                        let editable = pixel_fmt == crate::widegamut::WBGR;
                        let mut edit_hash = None;
                        let preview = if skip_previews {
                            None
                        } else {
                            celm.raw
                                .as_ref()
                                .and_then(|raw_bytes| {
                                    raw_to_rgba_any(raw_bytes, width, height, bpr, pixel_fmt)
                                })
                                .and_then(|px| {
                                    let pfile = rel_path("previews", i, &name, "png");
                                    codec::write_png(&out.join(&pfile), &px).ok()?;
                                    if editable {
                                        if let Ok(bytes) = fs::read(out.join(&pfile)) {
                                            edit_hash = Some(hash_bytes(&bytes));
                                        }
                                    }
                                    Some(pfile)
                                })
                        };
                        if preview.is_some() {
                            stats.dmp2_previews_ok += 1;
                            stats.dmp2_total += 1;
                        }
                        *stats.raw_payloads.entry(kind.clone()).or_default() += 1;
                        Content::RawPayload {
                            file,
                            kind,
                            preview,
                            edit_hash,
                        }
                    }
                    None => {
                        // Not re-encodable (deepmap2/rle/other): verbatim passthrough plus a preview when possible.
                        let file = write_raw_payload(out, i, &name, payload)?;
                        let kind = format!("celm-{}", compression::name(celm.compression));
                        let is_previewable = celm.compression == compression::DEEPMAP2
                            || celm.compression == compression::RLE;
                        if is_previewable && !skip_previews {
                            stats.dmp2_total += 1;
                            let editable =
                                pixel_fmt == pixel_format::ARGB || pixel_fmt == pixel_format::GA8;
                            let mut edit_hash = None;
                            let preview = decode_atlas_bitmap(csi).and_then(|px| {
                                let pfile = rel_path("previews", i, &name, "png");
                                codec::write_png(&out.join(&pfile), &px).ok()?;
                                if editable {
                                    if let Ok(bytes) = fs::read(out.join(&pfile)) {
                                        edit_hash = Some(hash_bytes(&bytes));
                                    }
                                }
                                Some(pfile)
                            });
                            if preview.is_some() {
                                stats.dmp2_previews_ok += 1;
                            }
                            *stats.raw_payloads.entry(kind.clone()).or_default() += 1;
                            Content::RawPayload {
                                file,
                                kind,
                                preview,
                                edit_hash,
                            }
                        } else {
                            *stats.raw_payloads.entry(kind.clone()).or_default() += 1;
                            Content::RawPayload {
                                file,
                                kind,
                                preview: None,
                                edit_hash: None,
                            }
                        }
                    }
                }
            }
        } else if is_rawd {
            if raw {
                let file = write_raw_payload(out, i, &name, payload)?;
                *stats.raw_payloads.entry("rawd".to_string()).or_default() += 1;
                Content::RawPayload {
                    file,
                    kind: "rawd".to_string(),
                    preview: None,
                    edit_hash: None,
                }
            } else {
                let (bytes, wrapped) = codec::rawd_decode(payload)
                    .with_context(|| format!("decoding RAWD payload for rendition {i} ({name})"))?;
                let ext = sniff_data_ext(&bytes);
                let file = rel_path("data", i, &name, ext);
                fs::write(out.join(&file), &bytes)?;
                stats.data += 1;
                Content::Data {
                    file,
                    lzfse: wrapped,
                }
            }
        } else if is_colr {
            if raw {
                let file = write_raw_payload(out, i, &name, payload)?;
                *stats.raw_payloads.entry("colr".to_string()).or_default() += 1;
                Content::RawPayload {
                    file,
                    kind: "colr".to_string(),
                    preview: None,
                    edit_hash: None,
                }
            } else {
                let c = codec::colr_decode(payload)
                    .with_context(|| format!("decoding COLR payload for rendition {i} ({name})"))?;
                stats.colors += 1;
                Content::Color {
                    color_space: c.color_space,
                    components: c.components,
                    system_color: c.system_name,
                    extra: B64.encode(&c.trailing),
                }
            }
        } else if payload.len() >= 4 && &payload[0..4] == b"ARGG" {
            if raw {
                let file = write_raw_payload(out, i, &name, payload)?;
                *stats.raw_payloads.entry("argg".to_string()).or_default() += 1;
                Content::RawPayload {
                    file,
                    kind: "argg".to_string(),
                    preview: None,
                    edit_hash: None,
                }
            } else if let Some(g) = crate::argg::decode(payload)
                .with_context(|| format!("decoding ARGG payload for rendition {i} ({name})"))?
            {
                stats.gradients += 1;
                Content::Gradient {
                    gradient_type: g.gradient_type,
                    reserved: g.reserved,
                    start: [g.start.0, g.start.1],
                    end: [g.end.0, g.end.1],
                    stops: g
                        .stops
                        .iter()
                        .map(|s| crate::manifest::GradientStopManifest {
                            location: s.location,
                            color_name: s.name_str(),
                        })
                        .collect(),
                }
            } else {
                let file = write_raw_payload(out, i, &name, payload)?;
                *stats.raw_payloads.entry("argg".to_string()).or_default() += 1;
                Content::RawPayload {
                    file,
                    kind: "argg".to_string(),
                    preview: None,
                    edit_hash: None,
                }
            }
        } else if let Some(container) = crate::rawimg::sniff(payload) {
            // Bare embedded image (JPEG/HEIF/PNG/PDF): verbatim RawPayload with an openable extension.
            let ext = container.ext();
            let file = write_raw_payload_ext(out, i, &name, payload, ext)?;
            let kind = format!("embedded-{ext}");
            *stats.raw_payloads.entry(kind.clone()).or_default() += 1;
            Content::RawPayload {
                file,
                kind,
                preview: None,
                edit_hash: None,
            }
        } else {
            let file = write_raw_payload(out, i, &name, payload)?;
            *stats.raw_payloads.entry("unknown".to_string()).or_default() += 1;
            Content::RawPayload {
                file,
                kind: "unknown".to_string(),
                preview: None,
                edit_hash: None,
            }
        };

        let values = &key_vecs[i];
        let key_map = key_vec_to_map(&key_ids, values);

        out_renditions.push(Rendition {
            key: key_map,
            name,
            layout: csi.header.layout,
            flags: csi.header.flags,
            pixel_format: pixel_format::name(pixel_fmt),
            color_space_id: csi.header.color_space_id,
            width,
            height,
            scale: csi.header.scale_factor,
            modified: csi.header.mod_time,
            slices,
            metrics,
            composition,
            bitmap_info,
            extra_tlvs,
            content,
        });
    }

    let manifest = Manifest {
        car: CarInfo {
            coreui_version: header.coreui_version,
            storage_version: header.storage_version,
            storage_timestamp: header.storage_timestamp,
            main_version_string: header.main_version_string,
            version_string: header.version_string,
            uuid: header.uuid_hex,
            associated_checksum: header.associated_checksum,
            schema_version: header.schema_version,
            color_space_id: header.color_space_id,
            key_semantics: header.key_semantics,
            key_format: key_names,
            metadata,
        },
        facets,
        appearances,
        localizations,
        renditions: out_renditions,
        bitmap_keys,
    };

    manifest.save(&out.join(crate::manifest::MANIFEST_NAME))?;

    println!("Decompiled {} -> {}", car.display(), out.display());
    println!("  renditions:  {}", manifest.renditions.len());
    println!("  facets:      {}", manifest.facets.len());
    println!("  appearances: {}", manifest.appearances.len());
    println!("  images:      {}", stats.images);
    println!("  data:        {}", stats.data);
    println!(
        "  links:       {} ({} with previews)",
        stats.links, stats.link_previews_ok
    );
    println!("  multisize:   {}", stats.multisize);
    println!("  colors:      {}", stats.colors);
    if stats.gradients > 0 {
        println!("  gradients:   {}", stats.gradients);
    }
    println!("  raw payloads:");
    for (kind, count) in &stats.raw_payloads {
        println!("    {kind:<20} {count}");
    }
    if stats.dmp2_total > 0 {
        println!(
            "  decoded previews (deepmap2/rle): {}/{}",
            stats.dmp2_previews_ok, stats.dmp2_total
        );
    }
    Ok(())
}

fn write_raw_payload(out: &Path, idx: usize, name: &str, data: &[u8]) -> Result<String> {
    write_raw_payload_ext(out, idx, name, data, "bin")
}

fn write_raw_payload_ext(
    out: &Path,
    idx: usize,
    name: &str,
    data: &[u8],
    ext: &str,
) -> Result<String> {
    let file = rel_path("rawpayload", idx, name, ext);
    let full: PathBuf = out.join(&file);
    fs::write(&full, data).with_context(|| format!("writing {}", full.display()))?;
    Ok(file)
}

fn sniff_data_ext(data: &[u8]) -> &'static str {
    // Text vector formats first (rawimg only detects binary containers).
    let head_len = data.len().min(16);
    let head = String::from_utf8_lossy(&data[..head_len]);
    let trimmed = head.trim_start();
    if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
        return "svg";
    }
    match crate::rawimg::sniff(data) {
        Some(c) => c.ext(),
        None => "bin",
    }
}
