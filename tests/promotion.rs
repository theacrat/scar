//! GA8 -> ARGB promotion: color edits of GA8 renditions must promote to ARGB instead of
//! collapsing to gray; grayscale edits keep GA8. Fully synthetic, no catalog fixture needed.

mod common;

use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-promotion-{}-{tag}", std::process::id()));
    d
}

fn gray_pixels(w: u32, h: u32) -> Pixels {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let g = (((x + y) * 255) / (w + h - 2).max(1)) as u8;
            rgba.extend_from_slice(&[g, g, g, 255]);
        }
    }
    Pixels {
        width: w,
        height: h,
        rgba,
    }
}

/// Author a one-asset GA8 catalog and return (decompiled dir, PNG rel path).
fn setup_ga8_catalog(tag: &str) -> (PathBuf, String) {
    let input = tmp(&format!("{tag}-in"));
    let packed = tmp(&format!("{tag}-packed"));
    let car = tmp(&format!("{tag}-1.car"));
    let dir = tmp(&format!("{tag}-a"));
    for d in [&input, &packed, &dir] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("glyph.png"), &gray_pixels(16, 16)).unwrap();

    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();
    scar::compile::compile(&packed, &car).unwrap();
    common::assert_assetutil_accepts(&car);
    scar::decompile::decompile(&car, &dir, false).unwrap();

    let manifest = Manifest::load(&dir.join("manifest.json")).unwrap();
    let rend = manifest
        .renditions
        .iter()
        .find(|r| matches!(r.content, Content::Image { .. }))
        .expect("packed catalog should contain the image rendition");
    assert_eq!(
        rend.pixel_format, "GA8",
        "a grayscale pack input should be stored as GA8"
    );
    let Content::Image { file, .. } = &rend.content else {
        unreachable!()
    };
    (dir, file.clone())
}

fn recompile_and_reload(dir: &Path, tag: &str) -> (PathBuf, Manifest) {
    let car = tmp(&format!("{tag}-2.car"));
    let out = tmp(&format!("{tag}-c"));
    let _ = std::fs::remove_dir_all(&out);
    scar::compile::compile(dir, &car).unwrap();
    common::assert_assetutil_accepts(&car);
    scar::decompile::decompile(&car, &out, false).unwrap();
    let manifest = Manifest::load(&out.join("manifest.json")).unwrap();
    let _ = std::fs::remove_file(&car);
    (out, manifest)
}

#[test]
fn color_edit_of_a_ga8_rendition_promotes_to_argb() {
    let (dir, png) = setup_ga8_catalog("color");

    // Opaque color edit, so the round-trip is exact.
    let mut edited = gray_pixels(16, 16);
    for (i, px) in edited.rgba.chunks_exact_mut(4).enumerate() {
        if i % 3 == 0 {
            px.copy_from_slice(&[200, 40, 90, 255]);
        }
    }
    codec::write_png(&dir.join(&png), &edited).unwrap();

    let (out, manifest) = recompile_and_reload(&dir, "color");
    let rend = manifest
        .renditions
        .iter()
        .find(|r| matches!(r.content, Content::Image { .. }))
        .unwrap();
    assert_eq!(
        rend.pixel_format, "ARGB",
        "colored edit must promote GA8 -> ARGB"
    );
    let Content::Image { file, .. } = &rend.content else {
        unreachable!()
    };
    let decoded = codec::read_png(&out.join(file)).unwrap();
    assert_eq!(
        decoded.rgba, edited.rgba,
        "the colored edit must survive the promotion exactly"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

/// One editable GA8 celm-rle rendition whose preview hash never matches, so compile always re-encodes from it.
fn setup_ga8_rle_catalog(tag: &str, preview: &Pixels) -> PathBuf {
    use std::collections::BTreeMap;

    let dir = tmp(&format!("{tag}-rle-dir"));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["rawpayload", "previews"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(
        dir.join("rawpayload/icon.bin"),
        b"placeholder, replaced by the re-encode",
    )
    .unwrap();
    codec::write_png(&dir.join("previews/icon.png"), preview).unwrap();

    let manifest = Manifest {
        car: common::synthetic_car_info(),
        facets: vec![],
        appearances: BTreeMap::new(),
        localizations: BTreeMap::new(),
        renditions: vec![scar::manifest::Rendition {
            key: [("element".to_string(), 1u16), ("identifier".to_string(), 1)]
                .into_iter()
                .collect(),
            name: "icon.png".to_string(),
            layout: 12,
            flags: 0,
            pixel_format: "GA8".to_string(),
            color_space_id: 1,
            width: preview.width,
            height: preview.height,
            scale: 100,
            modified: 0,
            slices: None,
            metrics: None,
            composition: None,
            bitmap_info: None,
            extra_tlvs: BTreeMap::new(),
            content: Content::RawPayload {
                file: "rawpayload/icon.bin".to_string(),
                kind: "celm-rle".to_string(),
                preview: Some("previews/icon.png".to_string()),
                edit_hash: Some("0000000000000000".to_string()),
            },
        }],
        bitmap_keys: BTreeMap::new(),
    };
    manifest.save(&dir.join("manifest.json")).unwrap();
    dir
}

/// CoreUI garbles ARGB RLE streams, so a promoted color edit must transcode to a plain LZFSE bitmap.
#[test]
fn color_edit_of_a_ga8_rle_rendition_promotes_and_transcodes_to_lzfse() {
    let mut edit = gray_pixels(16, 16);
    for (i, px) in edit.rgba.chunks_exact_mut(4).enumerate() {
        if i % 2 == 0 {
            px.copy_from_slice(&[210, 60, 30, 255]);
        }
    }
    let dir = setup_ga8_rle_catalog("color", &edit);
    let (out, manifest) = recompile_and_reload(&dir, "rle-color");

    let rend = &manifest.renditions[0];
    assert_eq!(
        rend.pixel_format, "ARGB",
        "colored RLE edit must promote to ARGB"
    );
    // LZFSE ARGB decodes straight to Image; celm-rle here would mean the forbidden RLE32 path was taken.
    let Content::Image {
        file, compression, ..
    } = &rend.content
    else {
        panic!(
            "promoted RLE edit must become a plain bitmap, got {:?}",
            rend.content
        );
    };
    assert_eq!(compression, "lzfse");
    let decoded = codec::read_png(&out.join(file)).unwrap();
    assert_eq!(
        decoded.rgba, edit.rgba,
        "the colored edit must survive the transcode exactly"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn grayscale_edit_of_a_ga8_rle_rendition_stays_native_rle() {
    let edit = gray_pixels(16, 16);
    let dir = setup_ga8_rle_catalog("gray", &edit);
    let (out, manifest) = recompile_and_reload(&dir, "rle-gray");

    let rend = &manifest.renditions[0];
    assert_eq!(rend.pixel_format, "GA8");
    let Content::RawPayload {
        kind,
        preview: Some(preview),
        ..
    } = &rend.content
    else {
        panic!(
            "grayscale RLE edit should stay a native RLE payload, got {:?}",
            rend.content
        );
    };
    assert_eq!(kind, "celm-rle");
    let decoded = codec::read_png(&out.join(preview)).unwrap();
    assert_eq!(
        decoded.rgba, edit.rgba,
        "the grayscale edit must survive the native RLE re-encode exactly"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn grayscale_edit_of_a_ga8_rendition_stays_ga8() {
    let (dir, png) = setup_ga8_catalog("gray");

    let mut edited = gray_pixels(16, 16);
    for px in edited.rgba.chunks_exact_mut(4) {
        px[0] = 255 - px[0];
        px[1] = px[0];
        px[2] = px[0];
    }
    codec::write_png(&dir.join(&png), &edited).unwrap();

    let (out, manifest) = recompile_and_reload(&dir, "gray");
    let rend = manifest
        .renditions
        .iter()
        .find(|r| matches!(r.content, Content::Image { .. }))
        .unwrap();
    assert_eq!(
        rend.pixel_format, "GA8",
        "a grayscale edit must keep the native GA8 format"
    );
    let Content::Image { file, .. } = &rend.content else {
        unreachable!()
    };
    let decoded = codec::read_png(&out.join(file)).unwrap();
    assert_eq!(
        decoded.rgba, edited.rgba,
        "the grayscale edit must survive re-encode exactly"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}
