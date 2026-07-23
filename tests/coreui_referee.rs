//! CoreUI as referee: CUICatalog dumps of a link-heavy asset must match scar's decompiled previews —
//! the only check that catches a wrong assumption shared by decompile and compile (e.g. the INLK y-flip).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "../examples/common/cuidump.rs"]
mod cuidump;

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-referee-{}-{tag}", std::process::id()));
    d
}

fn premultiplied(px: &Pixels) -> Vec<u8> {
    px.rgba
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            [
                ((p[0] as u32 * a + 127) / 255) as u8,
                ((p[1] as u32 * a + 127) / 255) as u8,
                ((p[2] as u32 * a + 127) / 255) as u8,
                p[3],
            ]
        })
        .collect()
}

/// Parse a cuidump .rgbaref: "RGBA" magic, u32 width, u32 height (LE), then premultiplied RGBA rows.
fn read_rgbaref(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || &data[0..4] != b"RGBA" {
        return None;
    }
    let w = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let h = u32::from_le_bytes(data[8..12].try_into().unwrap());
    (data.len() == 12 + (w * h * 4) as usize).then(|| (w, h, data[12..].to_vec()))
}

/// Max per-channel difference, or u32::MAX on length mismatch.
fn max_diff(a: &[u8], b: &[u8]) -> u32 {
    if a.len() != b.len() {
        return u32::MAX;
    }
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn coreui_renders_link_crops_matching_decompile_previews() {
    let car = Path::new("/Users/thea/Downloads/Assets.car");
    if !cfg!(target_os = "macos") || !car.exists() {
        eprintln!("not macOS or sample car not present, skipping CoreUI referee check");
        return;
    }

    let a = tmp("a");
    let dumps = tmp("dumps");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&dumps);
    std::fs::create_dir_all(&dumps).unwrap();

    scar::decompile::decompile(car, &a, false).unwrap();
    let m = Manifest::load(&a.join("manifest.json")).unwrap();

    // Pick the facet with the most editable link previews: crops depend on the y-origin convention.
    let mut link_count: BTreeMap<u16, usize> = BTreeMap::new();
    for r in &m.renditions {
        if let (
            Some(id),
            Content::Link {
                preview: Some(_),
                edit_hash: Some(_),
                ..
            },
        ) = (r.key.get("identifier"), &r.content)
        {
            *link_count.entry(*id).or_default() += 1;
        }
    }
    let Some((&facet_id, &n_links)) = link_count.iter().max_by_key(|(_, n)| **n) else {
        eprintln!("no editable links in sample, skipping");
        return;
    };
    let Some(facet) = m
        .facets
        .iter()
        .find(|f| f.attributes.get("identifier") == Some(&facet_id))
    else {
        eprintln!("link identifier {facet_id} has no facet, skipping");
        return;
    };
    if n_links < 5 {
        eprintln!("facet {:?} has only {n_links} links, skipping", facet.name);
        return;
    }

    let mut candidates: Vec<(String, u32, u32, Vec<u8>)> = Vec::new();
    for r in m
        .renditions
        .iter()
        .filter(|r| r.key.get("identifier") == Some(&facet_id))
    {
        let png = match &r.content {
            Content::Image { file, .. } => Some(file),
            Content::Link { preview, .. } | Content::RawPayload { preview, .. } => preview.as_ref(),
            _ => None,
        };
        if let Some(png) = png {
            let px = codec::read_png(&a.join(png)).unwrap();
            candidates.push((png.clone(), px.width, px.height, premultiplied(&px)));
        }
    }
    assert!(!candidates.is_empty());

    cuidump::dump(car, &dumps, Some(&facet.name));

    let mut compared = 0usize;
    let mut no_size_peer = 0usize;
    for entry in std::fs::read_dir(&dumps).unwrap() {
        let path = entry.unwrap().path();
        let Some((w, h, dump)) = read_rgbaref(&path) else {
            continue;
        };
        let peers: Vec<_> = candidates
            .iter()
            .filter(|(_, cw, ch, _)| (*cw, *ch) == (w, h))
            .collect();
        if peers.is_empty() {
            // cuidump may resolve to a variant scar has no rendition for (cross-scale substitution).
            no_size_peer += 1;
            continue;
        }
        // ±1: straight-alpha PNG -> re-premultiply rounding on soft edges.
        let best = peers
            .iter()
            .map(|(name, _, _, px)| (max_diff(px, &dump), name))
            .min()
            .unwrap();
        assert!(
            best.0 <= 1,
            "CoreUI's render of {:?} matches no decompiled file of asset {:?} (closest: {} at maxdiff {}) — \
             decompile/compile share a wrong pixel-region assumption",
            path.file_name().unwrap(),
            facet.name,
            best.1,
            best.0
        );
        compared += 1;
    }
    assert!(
        compared >= 5,
        "expected at least 5 comparable CoreUI dumps for {:?} (got {compared}, {no_size_peer} size-orphaned)",
        facet.name
    );
    eprintln!(
        "CoreUI referee: {compared} dumps of {:?} match scar previews (±1), {no_size_peer} size-orphaned",
        facet.name
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&dumps);
}
