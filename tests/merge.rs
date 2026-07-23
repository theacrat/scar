//! `scar::merge::merge_car`: bytes-in/bytes-out asset-pixel replacement.

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};
use scar::merge::{merge_car, merge_car_report};

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
