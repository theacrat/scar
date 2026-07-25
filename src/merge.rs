//! Zip-ignorant, bytes-in/bytes-out asset replacement: swap named assets'
//! contents in an existing `.car` without ever touching Apple tooling or archives.
//!
//! Each replacement is sniffed: PNG bytes replace a bitmap rendition's pixels,
//! anything else replaces a raw-data rendition's payload (SVG, PDF, arbitrary
//! RAWD data). A replacement PNG only lands on renditions whose (width, height)
//! exactly matches — scar does not resample; raw data lands verbatim and keeps
//! the rendition's existing LZFSE wrapping. Names that fit nothing are reported,
//! not fatal; the whole operation only fails if *every* replacement misses.
//!
//! With [`MergeOptions::add_missing`], an SVG or PDF whose name matches nothing
//! at all is instead *added* as a new vector asset (facet + data rendition).

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Result, bail};

use crate::authoring::{InstallOutcome, LinkPolicy, install_image, sanitize_name};
use crate::manifest::{Composition, Content, Facet, Manifest, Rendition};
use crate::{codec, compile, decompile, format};

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Every vector rendition in the shipping catalogs under `tests/re_catalogs`
/// (MapKit, Setup Assistant, Calculator) carries flags 4 and bitmap_info 1.
const VECTOR_FLAGS: u32 = 4;
const VECTOR_BITMAP_INFO: u32 = 1;

pub struct MergeReport {
    pub replaced: usize,
    pub unmatched: Vec<String>,
    /// Names added as brand-new assets; only ever non-empty with
    /// [`MergeOptions::add_missing`].
    pub added: Vec<String>,
}

/// Knobs for [`merge_car_report_with`].
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// Add an SVG or PDF replacement whose name matches nothing as a new asset
    /// instead of reporting it unmatched.
    pub add_missing: bool,
}

/// A vector payload scar knows how to add from scratch.
#[derive(Clone, Copy, PartialEq)]
enum Vector {
    Svg,
    Pdf,
}

impl Vector {
    fn sniff(bytes: &[u8]) -> Option<Self> {
        let trimmed = bytes.trim_ascii_start();
        if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
            Some(Self::Svg)
        } else if trimmed.starts_with(b"%PDF") {
            Some(Self::Pdf)
        } else {
            None
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Pdf => "pdf",
        }
    }

    fn pixel_format(self) -> &'static str {
        match self {
            Self::Svg => "SVG",
            Self::Pdf => "PDF",
        }
    }

    /// Shipping catalogs wrap SVG payloads in LZFSE and store PDFs raw.
    fn lzfse(self) -> bool {
        self == Self::Svg
    }
}

/// Rebuild `car` with the given `(asset-name, bytes)` replacements swapped in:
/// PNG bytes replace bitmap pixels, any other bytes replace a raw-data
/// rendition's payload.
/// Errors if every replacement was unmatched (see [`merge_car_report`]).
pub fn merge_car(car: &[u8], replacements: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let (bytes, report) = merge_car_report(car, replacements)?;
    if !replacements.is_empty() && report.replaced == 0 {
        bail!(
            "none of the {} replacement(s) matched a rendition: {:?}",
            replacements.len(),
            report.unmatched
        );
    }
    Ok(bytes)
}

/// Like [`merge_car`] but returns a [`MergeReport`] and never errors on misses.
pub fn merge_car_report(
    car: &[u8],
    replacements: &[(String, Vec<u8>)],
) -> Result<(Vec<u8>, MergeReport)> {
    merge_car_report_with(car, replacements, &MergeOptions::default())
}

/// [`merge_car_report`] with non-default [`MergeOptions`].
pub fn merge_car_report_with(
    car: &[u8],
    replacements: &[(String, Vec<u8>)],
    opts: &MergeOptions,
) -> Result<(Vec<u8>, MergeReport)> {
    let tmp = tempfile::TempDir::new()?;
    let in_car = tmp.path().join("in.car");
    let work = tmp.path().join("work");
    let out_car = tmp.path().join("out.car");
    fs::write(&in_car, car)?;

    decompile::decompile(&in_car, &work, false)?;
    let manifest_path = work.join(crate::manifest::MANIFEST_NAME);
    let mut manifest = Manifest::load(&manifest_path)?;

    let mut replaced = 0usize;
    let mut unmatched = Vec::new();
    let mut added = Vec::new();

    for (i, (name, bytes)) in replacements.iter().enumerate() {
        let idxs = resolve_renditions(&manifest, name);
        let mut landed = false;

        if bytes.starts_with(PNG_MAGIC) {
            let px = codec::decode_png(bytes)?;
            let png_path = tmp.path().join(format!("repl-{i}.png"));
            fs::write(&png_path, bytes)?;

            for idx in idxs {
                if let InstallOutcome::Installed = install_image(
                    &work,
                    &manifest.renditions[idx],
                    &png_path,
                    &px,
                    LinkPolicy::Paste,
                )? {
                    replaced += 1;
                    landed = true;
                }
            }
        } else {
            for idx in idxs {
                if let Content::Data { file, .. } = &manifest.renditions[idx].content {
                    fs::write(work.join(file), bytes)?;
                    replaced += 1;
                    landed = true;
                }
            }

            if !landed
                && opts.add_missing
                && let Some(kind) = Vector::sniff(bytes)
                && can_add(&manifest, name)
                && let Some(ident) = next_identifier(&manifest)
            {
                add_vector(&work, &mut manifest, name, bytes, kind, ident)?;
                added.push(name.clone());
                landed = true;
            }
        }

        if !landed {
            unmatched.push(name.clone());
        }
    }

    if !added.is_empty() {
        manifest.save(&manifest_path)?;
    }

    compile::compile(&work, &out_car)?;
    let bytes = fs::read(&out_car)?;
    Ok((
        bytes,
        MergeReport {
            replaced,
            unmatched,
            added,
        },
    ))
}

/// A name may only be added when it collides with nothing: no rendition and no
/// facet — not even a bitmap-only one, since grafting a vector rendition onto an
/// existing identifier has murky key-collision rules. The car's key format must
/// also be able to carry the key we would emit.
fn can_add(m: &Manifest, name: &str) -> bool {
    let carries = |attr: &str| m.car.key_format.iter().any(|k| k == attr);
    carries("element")
        && carries("identifier")
        && !m.facets.iter().any(|f| f.name == name)
        && resolve_renditions(m, name).is_empty()
}

/// Append a facet plus a layout-9 data rendition carrying `bytes`.
fn add_vector(
    work: &std::path::Path,
    m: &mut Manifest,
    name: &str,
    bytes: &[u8],
    kind: Vector,
    ident: u16,
) -> Result<()> {
    let file = reserve_data_file(work, name, kind)?;
    fs::write(work.join(&file), bytes)?;

    let mut key: BTreeMap<String, u16> = BTreeMap::new();
    key.insert("element".to_string(), ident);
    key.insert("identifier".to_string(), ident);
    if m.car.key_format.iter().any(|k| k == "scale") {
        key.insert("scale".to_string(), 1);
    }

    m.renditions.push(Rendition {
        key,
        name: format!("{name}.{}", kind.extension()),
        layout: format::layout::VECTOR,
        flags: VECTOR_FLAGS,
        pixel_format: kind.pixel_format().to_string(),
        color_space_id: 0,
        width: 0,
        height: 0,
        scale: 0,
        modified: 0,
        slices: None,
        metrics: None,
        composition: Some(Composition {
            blend_mode: 0,
            opacity: 1.0,
        }),
        bitmap_info: Some(VECTOR_BITMAP_INFO),
        extra_tlvs: BTreeMap::new(),
        content: Content::Data {
            file,
            lzfse: kind.lzfse(),
        },
    });

    let mut attributes = BTreeMap::new();
    attributes.insert("element".to_string(), ident);
    attributes.insert("identifier".to_string(), ident);
    m.facets.push(Facet {
        name: name.to_string(),
        hotspot: None,
        attributes,
    });
    Ok(())
}

/// Highest identifier in use across facets and rendition keys, plus one;
/// `None` once the u16 space is exhausted.
fn next_identifier(m: &Manifest) -> Option<u16> {
    let facets = m
        .facets
        .iter()
        .filter_map(|f| f.attributes.get("identifier"));
    let renditions = m.renditions.iter().filter_map(|r| r.key.get("identifier"));
    facets
        .chain(renditions)
        .copied()
        .max()
        .unwrap_or(0)
        .checked_add(1)
}

/// A free path under `data/`, creating the directory if absent.
fn reserve_data_file(work: &std::path::Path, name: &str, kind: Vector) -> Result<String> {
    fs::create_dir_all(work.join("data"))?;
    let stem = sanitize_name(name);
    let ext = kind.extension();
    for n in 0u32.. {
        let file = if n == 0 {
            format!("data/{stem}.{ext}")
        } else {
            format!("data/{stem}-{n}.{ext}")
        };
        if !work.join(&file).exists() {
            return Ok(file);
        }
    }
    unreachable!("the loop returns at the first free name")
}

/// Candidate rendition indices for `name`: prefer a facet's `identifier`, else
/// fall back to matching a rendition's CSI name (bare, or with `.png`, `.svg`
/// or `.pdf`).
fn resolve_renditions(m: &Manifest, name: &str) -> Vec<usize> {
    if let Some(facet) = m.facets.iter().find(|f| f.name == name)
        && let Some(ident) = facet.attributes.get("identifier")
    {
        let idxs: Vec<usize> = m
            .renditions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.key.get("identifier") == Some(ident))
            .map(|(i, _)| i)
            .collect();
        if !idxs.is_empty() {
            return idxs;
        }
    }
    let suffixed = [
        format!("{name}.png"),
        format!("{name}.svg"),
        format!("{name}.pdf"),
    ];
    m.renditions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.name == name || suffixed.contains(&r.name))
        .map(|(i, _)| i)
        .collect()
}
