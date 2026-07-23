//! Zip-ignorant, bytes-in/bytes-out asset replacement: swap named assets'
//! pixels in an existing `.car` without ever touching Apple tooling or archives.
//!
//! A replacement PNG only lands on renditions whose (width, height) exactly
//! matches — scar does not resample. Names that fit nothing are reported, not
//! fatal; the whole operation only fails if *every* replacement misses.

use std::fs;

use anyhow::{Result, bail};

use crate::authoring::{InstallOutcome, install_image};
use crate::manifest::Manifest;
use crate::{codec, compile, decompile};

pub struct MergeReport {
    pub replaced: usize,
    pub unmatched: Vec<String>,
}

/// Rebuild `car` with the given `(asset-name, PNG-bytes)` pixels swapped in.
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

    for (i, (name, png_bytes)) in replacements.iter().enumerate() {
        let px = codec::decode_png(png_bytes)?;
        // install_image copies from a path, so stage the bytes on disk.
        let png_path = tmp.path().join(format!("repl-{i}.png"));
        fs::write(&png_path, png_bytes)?;

        let mut landed = false;
        for idx in resolve_renditions(&manifest, name) {
            if let InstallOutcome::Installed =
                install_image(&work, &manifest.renditions[idx], &png_path, &px)?
            {
                replaced += 1;
                landed = true;
            }
        }
        if !landed {
            unmatched.push(name.clone());
        }
    }

    compile::compile(&work, &out_car)?;
    let bytes = fs::read(&out_car)?;
    Ok((bytes, MergeReport { replaced, unmatched }))
}

/// Candidate rendition indices for `name`: prefer a facet's `identifier`, else
/// fall back to matching a rendition's CSI name (bare or with `.png`).
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
    let png_name = format!("{name}.png");
    m.renditions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.name == name || r.name == png_name)
        .map(|(i, _)| i)
        .collect()
}
