//! Un-edited recompiles must write kept original payloads back verbatim: re-encoding semi-transparent
//! images through un/re-premultiply shifts edge pixels. Gated on the RE catalog fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use scar::manifest::{Content, Manifest};

fn setup_catalog() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs");
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("Setup"))
}

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-imgfid-{}-{tag}", std::process::id()));
    d
}

/// Map every rendition's key bytes -> CSI payload from a .car.
fn rendition_payloads(car: &Path) -> HashMap<Vec<u8>, Vec<u8>> {
    let data = std::fs::read(car).unwrap();
    let bom = scar::bom::Bom::parse(&data).unwrap();
    bom.tree_entries("RENDITIONS")
        .unwrap()
        .into_iter()
        .collect()
}

#[test]
fn unedited_semitransparent_images_pass_through_byte_exact() {
    let Some(car) = setup_catalog() else {
        eprintln!("no Setup fixture, skipping");
        return;
    };
    let a = tmp("a");
    let b = tmp("b.car");
    let _ = std::fs::remove_dir_all(&a);
    scar::decompile::decompile(&car, &a, false).unwrap();

    let manifest = Manifest::load(&a.join("manifest.json")).unwrap();
    let kept = manifest
        .renditions
        .iter()
        .filter(|r| {
            matches!(
                &r.content,
                Content::Image {
                    original: Some(_),
                    ..
                }
            )
        })
        .count();
    assert!(
        kept > 0,
        "expected at least one image with a kept original payload"
    );

    scar::compile::compile(&a, &b).unwrap();

    // Kept payloads must reappear byte-for-byte at the same key; without the fix they'd be re-encoded.
    let orig = rendition_payloads(&car);
    let rebuilt = rendition_payloads(&b);
    let mut checked = 0;
    let mut mismatches = 0;
    for (key, opayload) in &orig {
        if opayload.len() >= 4
            && &opayload[0..4] == b"ISTC"
            && csi_has_semitransparent_mlec(opayload)
        {
            checked += 1;
            match rebuilt.get(key) {
                Some(rpayload) if rpayload == opayload => {}
                _ => mismatches += 1,
            }
        }
    }
    assert!(checked > 0, "expected some MLEC image renditions to check");
    assert_eq!(mismatches, 0, "kept image payloads changed on recompile");

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_file(&b);
}

/// MLEC bitmap (comp 4/0, ARGB/GA8) with any semi-transparent pixel — the shape decompile keeps an original for.
fn csi_has_semitransparent_mlec(csi: &[u8]) -> bool {
    use scar::csi::Csi;
    let Ok(parsed) = Csi::parse(csi) else {
        return false;
    };
    if parsed.payload.len() < 12 || &parsed.payload[0..4] != b"MLEC" {
        return false;
    }
    let comp = u32::from_le_bytes(parsed.payload[8..12].try_into().unwrap());
    // Only the re-encodable bitmap compressions produce a kept original.
    if comp != 0 && comp != 4 {
        return false;
    }
    let bpp = match parsed.header.pixel_format {
        x if x == scar::format::pixel_format::ARGB => 4u32,
        x if x == scar::format::pixel_format::GA8 => 2u32,
        _ => return false,
    };
    let bpr = scar::format::bytes_per_row(parsed.header.width, bpp);
    let expected = bpr as usize * parsed.header.height as usize;
    let Ok(celm) = scar::codec::celm_decode(&parsed.payload, expected) else {
        return false;
    };
    let Some(raw) = celm.raw else { return false };
    let Ok(px) = scar::codec::raw_to_rgba(
        &raw,
        parsed.header.width,
        parsed.header.height,
        bpr,
        parsed.header.pixel_format,
    ) else {
        return false;
    };
    px.rgba.chunks_exact(4).any(|p| p[3] != 0 && p[3] != 255)
}
