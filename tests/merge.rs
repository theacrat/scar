//! `scar::merge::merge_car`: bytes-in/bytes-out asset replacement.

use std::collections::BTreeMap;

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest, Rendition};
use scar::merge::{merge_car, merge_car_report};

const SVG: &str = "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#123456\"/></svg>";

fn solid(w: u32, h: u32, color: [u8; 4]) -> Pixels {
    Pixels {
        width: w,
        height: h,
        rgba: color.repeat((w * h) as usize),
    }
}

fn png_bytes(px: &Pixels) -> Vec<u8> {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = tmp.path().join("x.png");
    codec::write_png(&p, px).unwrap();
    std::fs::read(&p).unwrap()
}

/// Build a two-asset catalog and return its `.car` bytes.
fn sample_car() -> Vec<u8> {
    let tmp = tempfile::TempDir::new().unwrap();
    let input = tmp.path().join("in");
    let packed = tmp.path().join("packed");
    let car = tmp.path().join("out.car");
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("logo.png"), &solid(24, 24, [10, 120, 240, 255])).unwrap();
    codec::write_png(&input.join("other.png"), &solid(8, 8, [128, 128, 128, 255])).unwrap();
    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();
    scar::compile::compile(&packed, &car).unwrap();
    std::fs::read(&car).unwrap()
}

/// [`sample_car`] plus a data rendition called `csi_name` (layout 1017), which
/// carries no facet so it is reachable only through the CSI-name fallback.
fn car_with_data(csi_name: &str, data: &[u8], lzfse: bool) -> Vec<u8> {
    let tmp = tempfile::TempDir::new().unwrap();
    let input = tmp.path().join("in");
    let packed = tmp.path().join("packed");
    let car = tmp.path().join("out.car");
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
    let file = format!("data/{csi_name}");
    std::fs::create_dir_all(packed.join("data")).unwrap();
    std::fs::write(packed.join(&file), data).unwrap();

    let mut key = BTreeMap::new();
    key.insert("element".to_string(), ident);
    key.insert("identifier".to_string(), ident);
    key.insert("scale".to_string(), 1);
    m.renditions.push(Rendition {
        key,
        name: csi_name.to_string(),
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
        content: Content::Data { file, lzfse },
    });
    m.save(&manifest_path).unwrap();

    scar::compile::compile(&packed, &car).unwrap();
    std::fs::read(&car).unwrap()
}

/// Decompile `.car` bytes to a fresh dir and hand back (dir, manifest).
fn decompiled(car: &[u8]) -> (tempfile::TempDir, Manifest) {
    let tmp = tempfile::TempDir::new().unwrap();
    let in_car = tmp.path().join("in.car");
    let work = tmp.path().join("work");
    std::fs::write(&in_car, car).unwrap();
    scar::decompile::decompile(&in_car, &work, false).unwrap();
    let m = Manifest::load(&work.join("manifest.json")).unwrap();
    (tmp, m)
}

fn decoded_asset(dir: &std::path::Path, m: &Manifest, name: &str) -> Vec<u8> {
    let facet = m.facets.iter().find(|f| f.name == name).unwrap();
    let ident = facet.attributes["identifier"];
    let r = m
        .renditions
        .iter()
        .find(|r| r.key.get("identifier") == Some(&ident))
        .unwrap();
    let Content::Image { file, .. } = &r.content else {
        panic!("expected image rendition")
    };
    let work = dir.join("work");
    codec::read_png(&work.join(file)).unwrap().rgba
}

/// The named data rendition's payload bytes and its `lzfse` flag.
fn decoded_data(dir: &std::path::Path, m: &Manifest, csi_name: &str) -> (Vec<u8>, bool) {
    let r = m.renditions.iter().find(|r| r.name == csi_name).unwrap();
    let Content::Data { file, lzfse } = &r.content else {
        panic!("expected data rendition, got {:?}", r.content)
    };
    (std::fs::read(dir.join("work").join(file)).unwrap(), *lzfse)
}

#[test]
fn merge_replaces_a_same_size_asset_and_leaves_others_untouched() {
    let car = sample_car();
    let new_logo = solid(24, 24, [250, 30, 60, 255]);

    // Baseline pixels of the asset we won't touch.
    let (base_dir, base_m) = decompiled(&car);
    let other_before = decoded_asset(base_dir.path(), &base_m, "other");

    let merged = merge_car(&car, &[("logo".to_string(), png_bytes(&new_logo))]).unwrap();

    let (dir, m) = decompiled(&merged);
    assert_eq!(
        decoded_asset(dir.path(), &m, "logo"),
        new_logo.rgba,
        "logo must decode to the replacement pixels"
    );
    assert_eq!(
        decoded_asset(dir.path(), &m, "other"),
        other_before,
        "the untouched asset's pixels must be identical"
    );
}

#[test]
fn wrong_size_replacement_is_unmatched_and_alone_errors() {
    let car = sample_car();
    let wrong = solid(10, 10, [1, 2, 3, 255]);

    let (_bytes, report) =
        merge_car_report(&car, &[("logo".to_string(), png_bytes(&wrong))]).unwrap();
    assert_eq!(report.replaced, 0);
    assert_eq!(report.unmatched, vec!["logo".to_string()]);

    let err = merge_car(&car, &[("logo".to_string(), png_bytes(&wrong))])
        .expect_err("all-unmatched must error");
    assert!(format!("{err:#}").contains("logo"), "error names the miss");
}

#[test]
fn unknown_name_is_unmatched() {
    let car = sample_car();
    let art = solid(24, 24, [9, 9, 9, 255]);
    let (_bytes, report) =
        merge_car_report(&car, &[("nope".to_string(), png_bytes(&art))]).unwrap();
    assert_eq!(report.replaced, 0);
    assert_eq!(report.unmatched, vec!["nope".to_string()]);
}

#[test]
fn partial_match_replaces_hits_and_reports_misses() {
    let car = sample_car();
    let new_logo = solid(24, 24, [7, 8, 9, 255]);
    let art = solid(24, 24, [1, 1, 1, 255]);

    let (merged, report) = merge_car_report(
        &car,
        &[
            ("logo".to_string(), png_bytes(&new_logo)),
            ("nope".to_string(), png_bytes(&art)),
        ],
    )
    .unwrap();
    assert_eq!(report.replaced, 1);
    assert_eq!(report.unmatched, vec!["nope".to_string()]);

    let (dir, m) = decompiled(&merged);
    assert_eq!(decoded_asset(dir.path(), &m, "logo"), new_logo.rgba);
}

#[test]
fn merge_replaces_an_svg_data_rendition_by_asset_name() {
    let car = car_with_data("glyph.svg", SVG.as_bytes(), false);
    let new_svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><circle cx=\"4\" cy=\"4\" r=\"3\" fill=\"#abcdef\"/></svg>".to_vec();

    let (base_dir, base_m) = decompiled(&car);
    let other_before = decoded_asset(base_dir.path(), &base_m, "other");

    let merged = merge_car(&car, &[("glyph".to_string(), new_svg.clone())]).unwrap();

    let (dir, m) = decompiled(&merged);
    let (data, _) = decoded_data(dir.path(), &m, "glyph.svg");
    assert_eq!(data, new_svg, "the data rendition must carry the new SVG");
    assert_eq!(
        decoded_asset(dir.path(), &m, "other"),
        other_before,
        "the untouched asset's pixels must be identical"
    );
    assert_eq!(
        decoded_asset(dir.path(), &m, "logo").len(),
        24 * 24 * 4,
        "the other bitmap is still a decodable 24x24"
    );
}

#[test]
fn merge_replaces_a_pdf_data_rendition_by_asset_name() {
    let car = car_with_data("chart.pdf", b"%PDF-1.4\n% original\n%%EOF\n", false);
    let new_pdf = b"%PDF-1.4\n% replacement\n%%EOF\n".to_vec();

    let merged = merge_car(&car, &[("chart".to_string(), new_pdf.clone())]).unwrap();

    let (dir, m) = decompiled(&merged);
    assert_eq!(decoded_data(dir.path(), &m, "chart.pdf").0, new_pdf);
}

#[test]
fn lzfse_wrapped_data_rendition_keeps_its_compression() {
    let car = car_with_data("glyph.svg", SVG.as_bytes(), true);
    let (base_dir, base_m) = decompiled(&car);
    assert!(
        decoded_data(base_dir.path(), &base_m, "glyph.svg").1,
        "fixture must start out LZFSE-wrapped"
    );

    let new_svg = SVG.replace("#123456", "#654321").repeat(4).into_bytes();
    let merged = merge_car(&car, &[("glyph".to_string(), new_svg.clone())]).unwrap();

    let (dir, m) = decompiled(&merged);
    let (data, lzfse) = decoded_data(dir.path(), &m, "glyph.svg");
    assert!(lzfse, "the rendition must still be LZFSE-wrapped");
    assert_eq!(data, new_svg);
}

#[test]
fn non_png_replacement_of_a_bitmap_only_name_is_unmatched() {
    let car = sample_car();
    let (bytes, report) =
        merge_car_report(&car, &[("logo".to_string(), SVG.as_bytes().to_vec())]).unwrap();
    assert_eq!(report.replaced, 0);
    assert_eq!(report.unmatched, vec!["logo".to_string()]);

    // The car still compiles and its bitmaps are untouched.
    let (base_dir, base_m) = decompiled(&car);
    let (dir, m) = decompiled(&bytes);
    assert_eq!(
        decoded_asset(dir.path(), &m, "logo"),
        decoded_asset(base_dir.path(), &base_m, "logo")
    );
}

#[test]
fn png_replacement_does_not_overwrite_a_data_rendition() {
    let car = car_with_data("glyph.svg", SVG.as_bytes(), false);
    let art = solid(24, 24, [3, 3, 3, 255]);

    let (bytes, report) =
        merge_car_report(&car, &[("glyph".to_string(), png_bytes(&art))]).unwrap();
    assert_eq!(report.replaced, 0);
    assert_eq!(report.unmatched, vec!["glyph".to_string()]);

    let (dir, m) = decompiled(&bytes);
    assert_eq!(decoded_data(dir.path(), &m, "glyph.svg").0, SVG.as_bytes());
}
