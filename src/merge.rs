//! Zip-ignorant, bytes-in/bytes-out asset replacement: swap named assets'
//! contents in an existing `.car` without ever touching Apple tooling or archives.
//!
//! Each replacement is sniffed: PNG bytes replace a bitmap rendition's pixels,
//! anything else replaces a raw-data rendition's payload (SVG, PDF, arbitrary
//! RAWD data). A replacement PNG only lands on renditions whose (width, height)
//! exactly matches — scar does not resample; raw data lands verbatim and keeps
//! the rendition's existing LZFSE wrapping. Names that fit nothing are reported,
//! not fatal; the whole operation only fails if *every* replacement misses.

use std::fs;

use anyhow::{Result, bail};

use crate::authoring::{InstallOutcome, LinkPolicy, install_image};
use crate::manifest::{Content, Manifest};
use crate::{codec, compile, decompile};

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

pub struct MergeReport {
    pub replaced: usize,
    pub unmatched: Vec<String>,
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
    let tmp = tempfile::TempDir::new()?;
    let in_car = tmp.path().join("in.car");
    let work = tmp.path().join("work");
    let out_car = tmp.path().join("out.car");
    fs::write(&in_car, car)?;

    decompile::decompile(&in_car, &work, false)?;
    let manifest = Manifest::load(&work.join(crate::manifest::MANIFEST_NAME))?;

    let mut replaced = 0usize;
    let mut unmatched = Vec::new();

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
        }

        if !landed {
            unmatched.push(name.clone());
        }
    }

    compile::compile(&work, &out_car)?;
    let bytes = fs::read(&out_car)?;
    Ok((
        bytes,
        MergeReport {
            replaced,
            unmatched,
        },
    ))
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
