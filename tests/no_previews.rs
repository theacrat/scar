//! `decompile --no-previews`: no preview PNGs, still repackable to the same bytes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::compile::compile;
use scar::decompile::{DecompileOptions, decompile, decompile_with};
use scar::manifest::{Content, Manifest, Rendition};

fn no_previews(car: &Path, out: &Path) {
    decompile_with(
        car,
        out,
        &DecompileOptions {
            raw: false,
            skip_previews: true,
        },
    )
    .unwrap();
}

fn previews_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir.join("previews")) {
        Ok(rd) => rd.count() == 0,
        Err(_) => true,
    }
}

#[test]
fn re_catalogs_no_previews_repack_identically() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs");
    if !dir.is_dir() {
        eprintln!("no tests/re_catalogs, skipping");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        // Every regular file in the directory is a catalog (some names are truncated).
        let entry = entry.unwrap();
        let car = entry.path();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let a = tmp.path().join(format!("{checked}a"));
        let b = tmp.path().join(format!("{checked}b"));
        let a_car = tmp.path().join(format!("{checked}a.car"));
        let b_car = tmp.path().join(format!("{checked}b.car"));

        decompile(&car, &a, false).unwrap_or_else(|e| panic!("decompile {car:?}: {e}"));
        no_previews(&car, &b);

        assert!(
            previews_empty(&b),
            "--no-previews wrote preview files for {car:?}"
        );
        assert!(
            b.join("manifest.json").is_file(),
            "--no-previews must still write a manifest for {car:?}"
        );

        compile(&a, &a_car).unwrap_or_else(|e| panic!("compile {car:?}: {e}"));
        compile(&b, &b_car).unwrap_or_else(|e| panic!("compile (no previews) {car:?}: {e}"));
        assert_eq!(
            std::fs::read(&a_car).unwrap(),
            std::fs::read(&b_car).unwrap(),
            "--no-previews repack differs from a normal repack for {car:?}"
        );

        checked += 1;
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        let _ = std::fs::remove_file(&a_car);
        let _ = std::fs::remove_file(&b_car);
    }
    eprintln!("no-previews round-trip verified {checked} catalogs");
}

const SVG: &str = "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#123456\"/></svg>";

fn solid(w: u32, h: u32, color: [u8; 4]) -> Pixels {
    Pixels {
        width: w,
        height: h,
        rgba: color.repeat((w * h) as usize),
    }
}

/// Two bitmap assets plus an SVG data rendition, compiled to a .car.
fn sample_car(tmp: &Path) -> PathBuf {
    let input = tmp.join("in");
    let packed = tmp.join("packed");
    let car = tmp.join("in.car");
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("logo.png"), &solid(24, 24, [10, 120, 240, 255])).unwrap();
    codec::write_png(&input.join("other.png"), &solid(8, 8, [128, 128, 128, 255])).unwrap();
    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();

    let manifest_path = packed.join("manifest.json");
    let mut m = Manifest::load(&manifest_path).unwrap();
    let ident = m
        .renditions
        .iter()
        .filter_map(|r| r.key.get("identifier"))
        .max()
        .copied()
        .unwrap_or(0)
        + 1;
    let file = "data/glyph.svg".to_string();
    std::fs::create_dir_all(packed.join("data")).unwrap();
    std::fs::write(packed.join(&file), SVG).unwrap();

    let mut key = BTreeMap::new();
    key.insert("element".to_string(), ident);
    key.insert("identifier".to_string(), ident);
    key.insert("scale".to_string(), 1);
    m.renditions.push(Rendition {
        key,
        name: "glyph.svg".to_string(),
        layout: 1017,
        flags: 0,
        pixel_format: "SVG".to_string(),
        color_space_id: 0,
        width: 0,
        height: 0,
        scale: 100,
        modified: 0,
        slices: None,
        metrics: None,
        composition: None,
        bitmap_info: None,
        extra_tlvs: BTreeMap::new(),
        content: Content::Data { file, lzfse: false },
    });
    m.save(&manifest_path).unwrap();

    compile(&packed, &car).unwrap();
    car
}

#[test]
fn synthetic_no_previews_repack_matches_full_round_trip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let car = sample_car(tmp.path());

    let full = tmp.path().join("full");
    decompile(&car, &full, false).unwrap();
    let from_full = tmp.path().join("from-full.car");
    compile(&full, &from_full).unwrap();

    let lean = tmp.path().join("lean");
    no_previews(&car, &lean);
    assert!(previews_empty(&lean), "--no-previews wrote preview files");
    assert!(lean.join("manifest.json").is_file());
    let from_lean = tmp.path().join("from-lean.car");
    compile(&lean, &from_lean).unwrap();

    assert_eq!(
        std::fs::read(&from_full).unwrap(),
        std::fs::read(&from_lean).unwrap(),
        "a --no-previews repack must be byte-identical to a full round trip"
    );
}
