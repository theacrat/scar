//! Round-trip the sample Assets.car: both decompiles must agree (census, byte-perfect
//! RawPayload/Data, near-pixel-perfect Images). Skips unless the sample car is present.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use scar::codec;
use scar::manifest::{Content, Manifest, Rendition};

fn content_kind(c: &Content) -> &'static str {
    match c {
        Content::Image { .. } => "image",
        Content::Data { .. } => "data",
        Content::Link { .. } => "link",
        Content::Multisize { .. } => "multisize",
        Content::Color { .. } => "color",
        Content::Gradient { .. } => "gradient",
        Content::RawPayload { .. } => "raw-payload",
    }
}

fn census(m: &Manifest) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for r in &m.renditions {
        *out.entry(content_kind(&r.content)).or_default() += 1;
    }
    out
}

fn by_key(m: &Manifest) -> BTreeMap<Vec<(String, u16)>, &Rendition> {
    m.renditions
        .iter()
        .map(|r| (r.key.iter().map(|(k, v)| (k.clone(), *v)).collect(), r))
        .collect()
}

#[test]
fn decompile_compile_decompile_round_trip() {
    let car = Path::new("/Users/thea/Downloads/Assets.car");
    if !car.exists() {
        eprintln!("sample Assets.car not present, skipping round-trip test");
        return;
    }

    let base = std::env::temp_dir().join(format!("scar-roundtrip-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir_a = base.join("a");
    let car_b = base.join("b.car");
    let dir_c = base.join("c");

    let result = std::panic::catch_unwind(|| {
        scar::decompile::decompile(car, &dir_a, false).expect("decompile (a) failed");
        scar::compile::compile(&dir_a, &car_b).expect("compile failed");
        common::assert_assetutil_accepts(&car_b);
        scar::decompile::decompile(&car_b, &dir_c, false).expect("decompile (c) failed");

        let manifest_a =
            Manifest::load(&dir_a.join(scar::manifest::MANIFEST_NAME)).expect("loading manifest a");
        let manifest_c =
            Manifest::load(&dir_c.join(scar::manifest::MANIFEST_NAME)).expect("loading manifest c");

        // Catalog-agnostic: assert a == c consistency, not a specific census (the sample may be any real car).
        assert!(
            !manifest_a.renditions.is_empty(),
            "catalog should have renditions"
        );
        assert_eq!(
            manifest_a.renditions.len(),
            manifest_c.renditions.len(),
            "rendition count must survive the round trip"
        );

        let census_a = census(&manifest_a);
        let census_c = census(&manifest_c);
        assert_eq!(
            census_a, census_c,
            "content-type census must match after round trip"
        );

        let map_a = by_key(&manifest_a);
        let map_c = by_key(&manifest_c);
        assert_eq!(
            map_a.len(),
            manifest_a.renditions.len(),
            "rendition keys must be unique in a"
        );
        assert_eq!(
            map_c.len(),
            manifest_c.renditions.len(),
            "rendition keys must be unique in c"
        );
        assert_eq!(
            map_a.keys().collect::<Vec<_>>(),
            map_c.keys().collect::<Vec<_>>(),
            "rendition key sets must match after round trip"
        );

        let mut raw_checked = 0usize;
        let mut data_checked = 0usize;
        let mut images_checked = 0usize;
        let mut images_exact = 0usize;
        let mut images_within_tolerance = 0usize;

        for (key, ra) in &map_a {
            let rc = map_c
                .get(key)
                .unwrap_or_else(|| panic!("key missing in c: {key:?}"));
            assert_eq!(
                content_kind(&ra.content),
                content_kind(&rc.content),
                "content type must match for key {key:?}"
            );

            match (&ra.content, &rc.content) {
                (Content::RawPayload { file: fa, .. }, Content::RawPayload { file: fc, .. }) => {
                    let ba = std::fs::read(dir_a.join(fa)).unwrap();
                    let bc = std::fs::read(dir_c.join(fc)).unwrap();
                    assert_eq!(
                        ba, bc,
                        "RawPayload bytes must be identical for {} ({fa} vs {fc})",
                        ra.name
                    );
                    raw_checked += 1;
                }
                (Content::Data { file: fa, .. }, Content::Data { file: fc, .. }) => {
                    let ba = std::fs::read(dir_a.join(fa)).unwrap();
                    let bc = std::fs::read(dir_c.join(fc)).unwrap();
                    assert_eq!(
                        ba, bc,
                        "Data bytes must be identical for {} ({fa} vs {fc})",
                        ra.name
                    );
                    data_checked += 1;
                }
                (Content::Image { file: fa, .. }, Content::Image { file: fc, .. }) => {
                    let pa = codec::read_png(&dir_a.join(fa)).unwrap();
                    let pc = codec::read_png(&dir_c.join(fc)).unwrap();
                    assert_eq!(pa.width, pc.width, "{}: width mismatch", ra.name);
                    assert_eq!(pa.height, pc.height, "{}: height mismatch", ra.name);
                    assert_eq!(
                        pa.rgba.len(),
                        pc.rgba.len(),
                        "{}: buffer size mismatch",
                        ra.name
                    );

                    let mut max_diff = 0i32;
                    for (pxa, pxc) in pa.rgba.chunks_exact(4).zip(pc.rgba.chunks_exact(4)) {
                        if pxa[3] == 0 && pxc[3] == 0 {
                            continue; // fully transparent pixels: don't care about RGB.
                        }
                        for i in 0..4 {
                            let d = (pxa[i] as i32 - pxc[i] as i32).abs();
                            max_diff = max_diff.max(d);
                        }
                    }
                    assert!(
                        max_diff <= 1,
                        "{}: pixel diff {max_diff} exceeds tolerance of 1",
                        ra.name
                    );
                    if max_diff == 0 {
                        images_exact += 1;
                    } else {
                        images_within_tolerance += 1;
                    }
                    images_checked += 1;
                }
                _ => {}
            }
        }

        // Tallies are catalog-specific; only require that images were exercised.
        let _ = (raw_checked, data_checked);
        assert!(
            images_checked > 0,
            "expected at least one Image rendition to compare"
        );

        eprintln!(
            "round-trip PNG comparison: {images_exact} byte-identical, {images_within_tolerance} within tolerance (<=1/channel), 0 worse"
        );
        eprintln!("RawPayload files byte-identical: {raw_checked}/{raw_checked}");
        eprintln!("Data files byte-identical: {data_checked}/{data_checked}");
    });

    let _ = std::fs::remove_dir_all(&base);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
