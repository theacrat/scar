//! `scar::merge::merge_car`: bytes-in/bytes-out asset replacement.

use std::collections::BTreeMap;

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest, Rendition};
use scar::merge::{
    MergeOptions, merge_car, merge_car_report, merge_car_report_with, replacement_sizes,
};

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

/// The name merge resolution would use to reach rendition `idx`, if any:
/// its facet's name, else its CSI name (stem) when no facet shadows it.
fn merge_name_for(m: &Manifest, idx: usize) -> Option<String> {
    let r = &m.renditions[idx];
    if let Some(ident) = r.key.get("identifier")
        && let Some(f) = m
            .facets
            .iter()
            .find(|f| f.attributes.get("identifier") == Some(ident))
    {
        return Some(f.name.clone());
    }
    let stem = r.name.strip_suffix(".png").unwrap_or(&r.name);
    (!m.facets.iter().any(|f| f.name == stem)).then(|| stem.to_string())
}

/// PNG replacements into atlas links or verbatim payloads rely on previews,
/// so the merge decompile must not skip them when a PNG is present.
/// Runs against the first shipping catalog with such a rendition; skips if none.
#[test]
fn png_into_preview_backed_rendition_survives_the_previewless_decompile() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no tests/re_catalogs, skipping");
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let car = std::fs::read(entry.path()).unwrap();
        let (_tmp, m) = decompiled(&car);
        let candidate = m.renditions.iter().enumerate().find_map(|(idx, r)| {
            let (w, h) = match &r.content {
                Content::Link {
                    rect,
                    preview: Some(_),
                    edit_hash: Some(_),
                    ..
                } => (rect[2], rect[3]),
                Content::RawPayload {
                    preview: Some(_),
                    edit_hash: Some(_),
                    ..
                } => (r.width, r.height),
                _ => return None,
            };
            if w == 0 || h == 0 {
                return None;
            }
            Some((merge_name_for(&m, idx)?, w, h))
        });
        let Some((name, w, h)) = candidate else {
            continue;
        };
        let png = png_bytes(&solid(w, h, [200, 40, 40, 255]));
        let (_, report) = merge_car_report(&car, &[(name.clone(), png)]).unwrap();
        assert!(
            report.replaced >= 1,
            "PNG for {name:?} ({w}x{h}) must land in {:?}",
            entry.path()
        );
        return;
    }
    eprintln!("no catalog with an editable link/raw-payload preview, skipping");
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
fn replacement_sizes_reports_each_assets_bitmap_dimensions() {
    let car = sample_car();
    let names = ["logo".to_string(), "other".to_string(), "nope".to_string()];
    let sizes = replacement_sizes(&car, &names).unwrap();

    assert_eq!(sizes.get("logo").map(Vec::as_slice), Some(&[(24, 24)][..]));
    assert_eq!(sizes.get("other").map(Vec::as_slice), Some(&[(8, 8)][..]));
    assert_eq!(sizes.get("nope"), None, "unknown names must be absent");
}

/// A data rendition has no pixels, so it must not be offered a size.
#[test]
fn replacement_sizes_skips_vector_assets() {
    let car = car_with_data("glyph.svg", SVG.as_bytes(), false);
    let sizes = replacement_sizes(&car, &["glyph".to_string()]).unwrap();
    assert_eq!(sizes.get("glyph"), None);
}

/// Renditions for `name` that `install_image` would accept a correctly-sized
/// PNG for: decoded bitmaps, and payloads or atlas crops with editable previews.
fn installable(m: &Manifest, name: &str) -> usize {
    let ident = m
        .facets
        .iter()
        .find(|f| f.name == name)
        .and_then(|f| f.attributes.get("identifier"));
    m.renditions
        .iter()
        .filter(|r| match ident {
            Some(i) => r.key.get("identifier") == Some(i),
            None => r.name == name || r.name.rsplit_once('.').map(|(s, _)| s) == Some(name),
        })
        .filter(|r| {
            matches!(
                &r.content,
                Content::Image { .. }
                    | Content::RawPayload {
                        preview: Some(_),
                        edit_hash: Some(_),
                        ..
                    }
                    | Content::Link {
                        preview: Some(_),
                        edit_hash: Some(_),
                        ..
                    }
            )
        })
        .count()
}

/// Offering a PNG at every reported size must reach every rendition that can
/// take one, so sizing is never what stands between a replacement and the car.
#[test]
fn every_size_a_rendition_can_take_is_reported() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no tests/re_catalogs, skipping");
        return;
    };
    let mut checked = 0;
    for entry in entries {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let car = std::fs::read(entry.path()).unwrap();
        let (_tmp, m) = decompiled(&car);
        let names: Vec<String> = m.facets.iter().map(|f| f.name.clone()).collect();
        let sizes = replacement_sizes(&car, &names).unwrap();

        // The asset with the most sizes exercises the multi-size path hardest.
        let Some((name, wh)) = sizes
            .iter()
            .filter(|(n, _)| installable(&m, n) > 0)
            .max_by_key(|(_, v)| v.len())
        else {
            continue;
        };
        let repl: Vec<(String, Vec<u8>)> = wh
            .iter()
            .map(|(w, h)| (name.clone(), png_bytes(&solid(*w, *h, [90, 160, 220, 255]))))
            .collect();
        let (_, report) = merge_car_report(&car, &repl).unwrap();
        assert_eq!(
            report.replaced,
            installable(&m, name),
            "every installable rendition of {name:?} in {:?} must be covered by \
             the {} reported size(s)",
            entry.path(),
            wh.len()
        );
        checked += 1;
        if checked == 3 {
            break;
        }
    }
    assert!(checked > 0, "no catalog exercised the sizing path");
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

#[test]
fn add_missing_adds_a_new_svg_asset() {
    let car = sample_car();
    let (base_dir, base_m) = decompiled(&car);
    let other_before = decoded_asset(base_dir.path(), &base_m, "other");

    let opts = MergeOptions { add_missing: true };
    let (merged, report) = merge_car_report_with(
        &car,
        &[("badge".to_string(), SVG.as_bytes().to_vec())],
        &opts,
    )
    .unwrap();
    assert_eq!(report.added, vec!["badge".to_string()]);
    assert_eq!(report.replaced, 0);
    assert!(report.unmatched.is_empty());

    let (dir, m) = decompiled(&merged);
    let facet = m
        .facets
        .iter()
        .find(|f| f.name == "badge")
        .expect("the new facet must exist");
    let r = m
        .renditions
        .iter()
        .find(|r| r.key.get("identifier") == facet.attributes.get("identifier"))
        .expect("the facet must resolve to a rendition");
    assert_eq!(r.name, "badge.svg");
    assert_eq!(r.layout, 9, "vector renditions use layout 9");
    assert_eq!(r.pixel_format, "SVG");

    let (data, lzfse) = decoded_data(dir.path(), &m, "badge.svg");
    assert_eq!(data, SVG.as_bytes(), "payload must be the added SVG");
    assert!(lzfse, "SVG payloads are LZFSE-wrapped, as in real catalogs");

    assert_eq!(
        decoded_asset(dir.path(), &m, "other"),
        other_before,
        "the untouched asset's pixels must be identical"
    );
    assert_eq!(decoded_asset(dir.path(), &m, "logo").len(), 24 * 24 * 4);
}

#[test]
fn add_missing_adds_a_new_pdf_asset() {
    let car = sample_car();
    let pdf = b"%PDF-1.4\n% added\n%%EOF\n".to_vec();

    let opts = MergeOptions { add_missing: true };
    let (merged, report) =
        merge_car_report_with(&car, &[("chart".to_string(), pdf.clone())], &opts).unwrap();
    assert_eq!(report.added, vec!["chart".to_string()]);
    assert_eq!(report.replaced, 0);
    assert!(report.unmatched.is_empty());

    let (dir, m) = decompiled(&merged);
    assert!(m.facets.iter().any(|f| f.name == "chart"));
    let r = m.renditions.iter().find(|r| r.name == "chart.pdf").unwrap();
    assert_eq!(r.layout, 9);
    assert_eq!(r.pixel_format, "PDF");

    let (data, lzfse) = decoded_data(dir.path(), &m, "chart.pdf");
    assert_eq!(data, pdf);
    assert!(!lzfse, "PDF payloads are stored raw, as in real catalogs");
}

#[test]
fn without_add_missing_a_new_svg_is_still_unmatched() {
    let car = sample_car();
    let repl = [("badge".to_string(), SVG.as_bytes().to_vec())];

    for (bytes, report) in [
        merge_car_report(&car, &repl).unwrap(),
        merge_car_report_with(&car, &repl, &MergeOptions::default()).unwrap(),
    ] {
        assert_eq!(report.replaced, 0);
        assert!(report.added.is_empty());
        assert_eq!(report.unmatched, vec!["badge".to_string()]);
        let (_dir, m) = decompiled(&bytes);
        assert!(m.facets.iter().all(|f| f.name != "badge"));
    }
}

#[test]
fn add_missing_will_not_graft_a_vector_onto_a_bitmap_name() {
    let car = sample_car();
    let (_base_dir, base_m) = decompiled(&car);

    let opts = MergeOptions { add_missing: true };
    let (merged, report) = merge_car_report_with(
        &car,
        &[("logo".to_string(), SVG.as_bytes().to_vec())],
        &opts,
    )
    .unwrap();
    assert_eq!(report.replaced, 0);
    assert!(report.added.is_empty());
    assert_eq!(report.unmatched, vec!["logo".to_string()]);

    let (_dir, m) = decompiled(&merged);
    assert_eq!(m.facets.len(), base_m.facets.len());
    assert_eq!(m.renditions.len(), base_m.renditions.len());
}

#[test]
fn add_missing_ignores_bytes_that_are_neither_svg_nor_pdf() {
    let car = sample_car();
    let (_base_dir, base_m) = decompiled(&car);

    let opts = MergeOptions { add_missing: true };
    let (merged, report) = merge_car_report_with(
        &car,
        &[("blob".to_string(), b"\x00\x01\x02not a vector".to_vec())],
        &opts,
    )
    .unwrap();
    assert_eq!(report.replaced, 0);
    assert!(report.added.is_empty());
    assert_eq!(report.unmatched, vec!["blob".to_string()]);

    let (_dir, m) = decompiled(&merged);
    assert_eq!(m.facets.len(), base_m.facets.len());
    assert_eq!(m.renditions.len(), base_m.renditions.len());
}

#[test]
fn an_added_asset_can_be_replaced_again() {
    let car = sample_car();
    let opts = MergeOptions { add_missing: true };
    let (added, report) = merge_car_report_with(
        &car,
        &[("badge".to_string(), SVG.as_bytes().to_vec())],
        &opts,
    )
    .unwrap();
    assert_eq!(report.added, vec!["badge".to_string()]);

    let new_svg = SVG.replace("#123456", "#654321").into_bytes();
    let (merged, report) =
        merge_car_report(&added, &[("badge".to_string(), new_svg.clone())]).unwrap();
    assert_eq!(report.replaced, 1);
    assert!(report.unmatched.is_empty());
    assert!(report.added.is_empty());

    let (dir, m) = decompiled(&merged);
    let (data, lzfse) = decoded_data(dir.path(), &m, "badge.svg");
    assert_eq!(data, new_svg);
    assert!(lzfse, "the added rendition keeps its LZFSE wrapping");
}

/// A car whose key format cannot express `element` must not gain an
/// unencodable key: the add is skipped and the name stays unmatched.
#[test]
fn add_missing_skips_a_car_whose_key_format_lacks_element() {
    let tmp = tempfile::TempDir::new().unwrap();
    let input = tmp.path().join("in");
    let packed = tmp.path().join("packed");
    let car_path = tmp.path().join("out.car");
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("logo.png"), &solid(24, 24, [10, 120, 240, 255])).unwrap();
    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();

    let manifest_path = packed.join("manifest.json");
    let mut m = Manifest::load(&manifest_path).unwrap();
    m.car.key_format.retain(|k| k != "element");
    m.save(&manifest_path).unwrap();
    scar::compile::compile(&packed, &car_path).unwrap();
    let car = std::fs::read(&car_path).unwrap();

    let opts = MergeOptions { add_missing: true };
    let (merged, report) = merge_car_report_with(
        &car,
        &[("badge".to_string(), SVG.as_bytes().to_vec())],
        &opts,
    )
    .unwrap();
    assert!(report.added.is_empty());
    assert_eq!(report.unmatched, vec!["badge".to_string()]);

    let (_dir, out) = decompiled(&merged);
    assert!(out.facets.iter().all(|f| f.name != "badge"));
}
