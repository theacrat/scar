//! End-to-end test of the edit workflow: decompile a catalog, modify a
//! decoded preview PNG of a natively-re-encodable rendition (deepmap2 palette),
//! recompile, and confirm the edit is applied (the rendition re-decodes to the
//! edited pixels) while the payload is a valid CELM. Gated on the RE catalog
//! fixture being present.

use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};

fn setup_catalog() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs");
    if !dir.is_dir() {
        return None;
    }
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("Setup"))
}

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-edit-{}-{tag}", std::process::id()));
    d
}

#[test]
fn editing_a_deepmap2_palette_rendition_applies_the_edit() {
    let Some(car) = setup_catalog() else {
        eprintln!("no Setup catalog fixture, skipping");
        return;
    };
    let a = tmp("a");
    let b = tmp("b.car");
    let c = tmp("c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(&car, &a, false).unwrap();
    let manifest = Manifest::load(&a.join("manifest.json")).unwrap();

    let target = manifest.renditions.iter().find(|r| {
        matches!(&r.content, Content::RawPayload { kind, edit_hash: Some(_), preview: Some(_), .. } if kind == "celm-deepmap2")
            && r.pixel_format == "ARGB"
    });
    let Some(rend) = target else {
        eprintln!("no editable deepmap2 rendition in fixture, skipping");
        return;
    };
    let Content::RawPayload {
        preview: Some(preview),
        ..
    } = &rend.content
    else {
        unreachable!()
    };
    let (w, h) = (rend.width, rend.height);

    // Paint a deterministic <=256-color image (palette-lossless) over the preview.
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            if (x + y) % 17 == 0 {
                rgba.extend_from_slice(&[220, 20, 20, 255]);
            } else {
                rgba.extend_from_slice(&[30, 30, 200, 255]);
            }
        }
    }
    let edited = Pixels {
        width: w,
        height: h,
        rgba,
    };
    codec::write_png(&a.join(preview), &edited).unwrap();

    // Compile detects the edit via the changed preview hash.
    scar::compile::compile(&a, &b).unwrap();
    scar::decompile::decompile(&b, &c, false).unwrap();

    let m2 = Manifest::load(&c.join("manifest.json")).unwrap();
    let r2 = m2
        .renditions
        .iter()
        .find(|r| r.name == rend.name && r.width == w && r.height == h)
        .expect("edited rendition present after round-trip");
    let Content::RawPayload {
        preview: Some(p2), ..
    } = &r2.content
    else {
        panic!("edited rendition should still be a previewable raw-payload");
    };
    let decoded = codec::read_png(&c.join(p2)).unwrap();
    assert_eq!(decoded.width, w);
    assert_eq!(decoded.height, h);
    // <=256 colors → palette encode is lossless → exact match.
    assert_eq!(
        decoded.rgba, edited.rgba,
        "edited pixels did not survive re-encode"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn editing_a_richer_deepmap2_rendition_uses_native_default() {
    // A >256-color edit takes the native deepmap2 "default" (YCoCg-R) encoder
    // rather than palette. The default codec is lossless for on-lattice colors
    // and within a small delta otherwise; assert the edit survives closely and
    // the rendition stays a deepmap2 CELM payload.
    let Some(car) = setup_catalog() else {
        eprintln!("no Setup catalog fixture, skipping");
        return;
    };
    let a = tmp("da");
    let b = tmp("db.car");
    let c = tmp("dc");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(&car, &a, false).unwrap();
    let manifest = Manifest::load(&a.join("manifest.json")).unwrap();
    let target = manifest.renditions.iter().find(|r| {
        matches!(&r.content, Content::RawPayload { kind, edit_hash: Some(_), preview: Some(_), .. } if kind == "celm-deepmap2")
            && r.pixel_format == "ARGB"
            && r.width >= 8
            && r.height >= 8
    });
    let Some(rend) = target else {
        eprintln!("no editable deepmap2 rendition, skipping");
        return;
    };
    let Content::RawPayload {
        preview: Some(preview),
        ..
    } = &rend.content
    else {
        unreachable!()
    };
    let (w, h) = (rend.width, rend.height);

    // Smooth gradient — hundreds of distinct colors → native default codec.
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            let bl = (((x + y) * 255) / (w + h)) as u8;
            rgba.extend_from_slice(&[r, g, bl, 255]);
        }
    }
    let edited = Pixels {
        width: w,
        height: h,
        rgba,
    };
    codec::write_png(&a.join(preview), &edited).unwrap();

    scar::compile::compile(&a, &b).unwrap();
    scar::decompile::decompile(&b, &c, false).unwrap();

    let m2 = Manifest::load(&c.join("manifest.json")).unwrap();
    let r2 = m2
        .renditions
        .iter()
        .find(|r| r.name == rend.name && r.width == w && r.height == h)
        .unwrap();
    let Content::RawPayload {
        kind,
        preview: Some(p2),
        ..
    } = &r2.content
    else {
        panic!("edited rendition should remain a raw-payload");
    };
    assert_eq!(
        kind, "celm-deepmap2",
        "richer edit should stay a native deepmap2 payload"
    );
    let decoded = codec::read_png(&c.join(p2)).unwrap();
    assert_eq!((decoded.width, decoded.height), (w, h));
    let maxd = decoded
        .rgba
        .iter()
        .zip(&edited.rgba)
        .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(maxd <= 2, "native-default edit drifted by {maxd} (> 2)");

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}
