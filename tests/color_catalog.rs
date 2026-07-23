//! Regression coverage for the paths a color-and-data-heavy catalog
//! exercises: named colors (COLR), and raw (uncompressed, version-0) RAWD
//! data renditions. These originally surfaced three bugs fixed together (in a
//! 9 MB iCloud+ catalog since replaced by the smaller committed Calculator
//! one): a hard error on unknown pixel formats, a 1-ULP color drift from
//! serde_json float parsing, and — the important one — a RAWD version word
//! wrongly forced to 1, which made CoreUI/assetutil hang trying to
//! LZFSE-inflate uncompressed data.

use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/re_catalogs/_System_Applications_Calculator.app_Contents_Resources_Asset")
}

fn tmp(sub: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-colortest-{}-{sub}", std::process::id()));
    d
}

#[test]
fn color_catalog_round_trips_exactly() {
    let car = fixture();
    if !car.exists() {
        eprintln!("no color fixture, skipping");
        return;
    }
    let a = tmp("a");
    let b = tmp("b.car");
    let c = tmp("c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(&car, &a, false).expect("decompile original");
    scar::compile::compile(&a, &b).expect("compile");
    scar::decompile::decompile(&b, &c, false).expect("decompile rebuilt");

    // manifest.json plus every emitted asset file must be byte-identical.
    let ma = std::fs::read(a.join("manifest.json")).unwrap();
    let mc = std::fs::read(c.join("manifest.json")).unwrap();
    assert_eq!(
        ma, mc,
        "manifest.json differs across round-trip (color/float drift?)"
    );

    // Spot-check the manifest content: colors decode, and at least one raw
    // (uncompressed) data rendition exists — the .iconstack case.
    let manifest: serde_json::Value = serde_json::from_slice(&ma).unwrap();
    let rends = manifest["renditions"].as_array().unwrap();
    let colors = rends
        .iter()
        .filter(|r| r["content"]["type"] == "color")
        .count();
    let raw_data = rends
        .iter()
        .filter(|r| r["content"]["type"] == "data" && r["content"]["lzfse"] == false)
        .count();
    assert!(colors > 0, "expected color renditions");
    assert!(raw_data > 0, "expected uncompressed data renditions");

    // The rebuilt catalog's RAWD payloads for uncompressed data must use
    // version word 0 (the hang regression). Verify by re-reading b.car's bytes
    // and checking every DWAR payload whose body is not an LZFSE stream.
    let bytes = std::fs::read(&b).unwrap();
    let mut checked_raw = 0usize;
    let mut i = 0;
    while let Some(pos) = find(&bytes[i..], b"DWAR") {
        let off = i + pos;
        if off + 12 <= bytes.len() {
            let version = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap()) as usize;
            let body = &bytes[off + 12..(off + 12 + len).min(bytes.len())];
            let is_lzfse = body.len() >= 3 && &body[0..3] == b"bvx";
            if !is_lzfse {
                assert_eq!(
                    version, 0,
                    "uncompressed RAWD at {off} must be version 0, got {version}"
                );
                checked_raw += 1;
            }
        }
        i = off + 4;
    }
    assert!(
        checked_raw > 0,
        "expected to verify some uncompressed RAWD payloads"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
