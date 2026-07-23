//! `scar pack`: build a decompiled-form directory from a plain folder of PNGs
//! (no existing .car needed) and verify it survives compile -> decompile intact.

use std::collections::BTreeSet;
use std::path::Path;

use scar::authoring::{self, PackOptions};
use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};

/// Opaque solid PNG: full alpha makes the premultiply round trip bit-exact, allowing byte-identical asserts.
fn write_test_png(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
    codec::write_png(
        path,
        &Pixels {
            width: w,
            height: h,
            rgba,
        },
    )
    .expect("write_test_png");
}

fn write_png_fn(path: &Path, w: u32, h: u32, mut f: impl FnMut(u32, u32) -> [u8; 4]) {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&f(x, y));
        }
    }
    codec::write_png(
        path,
        &Pixels {
            width: w,
            height: h,
            rgba,
        },
    )
    .expect("write_png_fn");
}

#[test]
fn pack_infers_ga8_for_grayscale_and_argb_for_colour() {
    let base = std::env::temp_dir().join(format!("scar-authoring-pf-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let input = base.join("input");
    let out = base.join("packed");
    let car_path = base.join("out.car");
    let verify = base.join("verify");
    std::fs::create_dir_all(&input).unwrap();

    write_png_fn(&input.join("Gray.png"), 8, 8, |x, _| {
        let v = (x * 32) as u8;
        [v, v, v, 255]
    });
    write_png_fn(&input.join("Colour.png"), 8, 8, |x, y| {
        [(x * 32) as u8, (y * 32) as u8, 128, 255]
    });

    let result = std::panic::catch_unwind(|| {
        authoring::pack(&input, &out, &PackOptions::default()).expect("pack failed");
        let manifest =
            Manifest::load(&out.join(scar::manifest::MANIFEST_NAME)).expect("loading manifest");

        let gray = manifest
            .renditions
            .iter()
            .find(|r| r.name.starts_with("Gray"))
            .expect("Gray rendition");
        let colour = manifest
            .renditions
            .iter()
            .find(|r| r.name.starts_with("Colour"))
            .expect("Colour rendition");
        assert_eq!(
            gray.pixel_format, "GA8",
            "grayscale image should be inferred as GA8"
        );
        assert_eq!(
            colour.pixel_format, "ARGB",
            "colour image should be inferred as ARGB"
        );

        scar::compile::compile(&out, &car_path).expect("compile failed");
        scar::decompile::decompile(&car_path, &verify, false).expect("decompile failed");
        let vm = Manifest::load(&verify.join(scar::manifest::MANIFEST_NAME))
            .expect("loading verify manifest");

        for name in ["Gray", "Colour"] {
            let packed = manifest
                .renditions
                .iter()
                .find(|r| r.name.starts_with(name))
                .unwrap();
            let round = vm
                .renditions
                .iter()
                .find(|r| r.name.starts_with(name))
                .expect("round-tripped rendition");
            let Content::Image { file: pf, .. } = &packed.content else {
                panic!("packed not image")
            };
            let Content::Image { file: rf, .. } = &round.content else {
                panic!("round not image")
            };
            let a = codec::read_png(&out.join(pf)).unwrap();
            let b = codec::read_png(&verify.join(rf)).unwrap();
            assert_eq!(a.rgba, b.rgba, "{name}: pixels must round-trip identically");
        }
    });

    let _ = std::fs::remove_dir_all(&base);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn pack_compile_decompile_round_trip() {
    let base = std::env::temp_dir().join(format!("scar-authoring-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let input = base.join("input");
    let out = base.join("packed");
    let car_path = base.join("out.car");
    let verify = base.join("verify");

    std::fs::create_dir_all(&input).unwrap();

    // The three pack input styles: plain 1x PNG, @2x-suffixed PNG, and an .imageset with Contents.json.
    write_test_png(&input.join("Solid.png"), 4, 4, [200, 30, 30]);
    write_test_png(&input.join("Icon@2x.png"), 8, 8, [30, 200, 30]);
    let imageset = input.join("Star.imageset");
    std::fs::create_dir_all(&imageset).unwrap();
    write_test_png(&imageset.join("star.png"), 6, 6, [30, 30, 200]);
    std::fs::write(
        imageset.join("Contents.json"),
        r#"{
  "images": [
    {
      "filename": "star.png",
      "idiom": "universal",
      "scale": "1x"
    }
  ],
  "info": {
    "author": "xcode",
    "version": 1
  }
}
"#,
    )
    .unwrap();

    let result = std::panic::catch_unwind(|| {
        authoring::pack(&input, &out, &PackOptions::default()).expect("pack failed");

        let manifest_path = out.join(scar::manifest::MANIFEST_NAME);
        assert!(
            manifest_path.exists(),
            "manifest.json should exist in the packed output"
        );

        let manifest = Manifest::load(&manifest_path).expect("loading packed manifest.json");
        assert_eq!(
            manifest.renditions.len(),
            3,
            "expected 3 renditions (Solid, Icon@2x, Star)"
        );
        assert_eq!(
            manifest.facets.len(),
            3,
            "expected 3 facets, one per asset name"
        );
        assert_eq!(manifest.car.key_format, authoring::default_key_format());
        assert_eq!(manifest.car.key_format.len(), 12);
        assert_eq!(
            manifest.appearances.get("UIAppearanceAny").copied(),
            Some(0)
        );

        for r in &manifest.renditions {
            assert!(
                matches!(r.content, Content::Image { .. }),
                "{}: expected Image content",
                r.name
            );
        }

        let keys: BTreeSet<Vec<(String, u16)>> = manifest
            .renditions
            .iter()
            .map(|r| r.key.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .collect();
        assert_eq!(keys.len(), 3, "rendition keys must be unique");

        let elements: BTreeSet<u16> = manifest
            .renditions
            .iter()
            .map(|r| *r.key.get("element").expect("element key"))
            .collect();
        assert_eq!(elements.len(), 3, "expected 3 distinct element ids");

        scar::compile::compile(&out, &car_path).expect("compile failed");
        assert!(car_path.exists());

        scar::decompile::decompile(&car_path, &verify, false).expect("decompile failed");
        let verify_manifest = Manifest::load(&verify.join(scar::manifest::MANIFEST_NAME))
            .expect("loading verify manifest.json");

        let verify_images: Vec<_> = verify_manifest
            .renditions
            .iter()
            .filter(|r| matches!(r.content, Content::Image { .. }))
            .collect();
        assert_eq!(
            verify_images.len(),
            3,
            "expected 3 Image renditions to survive the round trip"
        );

        let mut checked = 0usize;
        for packed in manifest.renditions.iter() {
            let Content::Image {
                file: packed_file, ..
            } = &packed.content
            else {
                continue;
            };
            let verify_rend = verify_images
                .iter()
                .find(|r| r.name == packed.name)
                .unwrap_or_else(|| panic!("{}: missing from decompiled output", packed.name));
            let Content::Image {
                file: verify_file, ..
            } = &verify_rend.content
            else {
                panic!(
                    "{}: expected Image content in decompiled output",
                    packed.name
                )
            };

            let original = codec::read_png(&out.join(packed_file)).expect("reading packed png");
            let round_tripped =
                codec::read_png(&verify.join(verify_file)).expect("reading round-tripped png");

            assert_eq!(
                original.width, round_tripped.width,
                "{}: width mismatch",
                packed.name
            );
            assert_eq!(
                original.height, round_tripped.height,
                "{}: height mismatch",
                packed.name
            );
            assert_eq!(
                original.rgba, round_tripped.rgba,
                "{}: pixels must be identical (fully opaque source)",
                packed.name
            );
            checked += 1;
        }
        assert_eq!(
            checked, 3,
            "expected to verify pixel-identity for all 3 packed images"
        );
    });

    let _ = std::fs::remove_dir_all(&base);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
