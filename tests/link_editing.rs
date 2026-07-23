//! Editable link (INLK) previews: a changed preview is pasted into the target atlas
//! at the link's rect on compile. Most tests skip unless the sample Assets.car is present.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest, Rendition};

fn sample_car() -> Option<&'static Path> {
    let car = Path::new("/Users/thea/Downloads/Assets.car");
    car.exists().then_some(car)
}

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-linkedit-{}-{tag}", std::process::id()));
    d
}

/// Full key vector (key_format order, absent attributes = 0), matching how compile/decompile resolve link targets.
fn full_key(key_format: &[String], attrs: &BTreeMap<String, u16>) -> Vec<u16> {
    key_format
        .iter()
        .map(|name| attrs.get(name).copied().unwrap_or(0))
        .collect()
}

fn find_rendition<'m>(m: &'m Manifest, key: &[u16]) -> Option<&'m Rendition> {
    m.renditions
        .iter()
        .find(|r| full_key(&m.car.key_format, &r.key) == key)
}

/// First editable link whose target atlas satisfies `atlas_ok`; returns (link index, atlas index).
fn find_link(m: &Manifest, atlas_ok: impl Fn(&Rendition) -> bool) -> Option<(usize, usize)> {
    m.renditions.iter().enumerate().find_map(|(i, r)| {
        let Content::Link {
            target,
            preview: Some(_),
            edit_hash: Some(_),
            ..
        } = &r.content
        else {
            return None;
        };
        let want = full_key(&m.car.key_format, target);
        let atlas_idx = m
            .renditions
            .iter()
            .position(|a| full_key(&m.car.key_format, &a.key) == want)?;
        atlas_ok(&m.renditions[atlas_idx]).then_some((i, atlas_idx))
    })
}

fn checker(w: u32, h: u32, a: [u8; 4], b: [u8; 4]) -> Pixels {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(if (x / 2 + y / 2) % 2 == 0 { &a } else { &b });
        }
    }
    Pixels {
        width: w,
        height: h,
        rgba,
    }
}

fn max_channel_diff(a: &Pixels, b: &Pixels) -> u32 {
    a.rgba
        .iter()
        .zip(&b.rgba)
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn edited_link_preview_is_pasted_into_an_image_atlas() {
    let Some(car) = sample_car() else {
        eprintln!("sample Assets.car not present, skipping");
        return;
    };
    let a = tmp("img-a");
    let b = tmp("img-b.car");
    let c = tmp("img-c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(car, &a, false).unwrap();
    let m = Manifest::load(&a.join("manifest.json")).unwrap();

    let Some((link_idx, atlas_idx)) = find_link(&m, |atlas| {
        matches!(atlas.content, Content::Image { .. }) && atlas.pixel_format == "ARGB"
    }) else {
        eprintln!("no editable link into a plain-image atlas, skipping");
        return;
    };
    let link = &m.renditions[link_idx];
    let Content::Link {
        target,
        rect,
        preview: Some(preview),
        ..
    } = &link.content
    else {
        unreachable!()
    };
    let target_key = full_key(&m.car.key_format, target);

    // A second, untouched link into the same atlas (to prove pastes are local).
    let neighbor = m.renditions.iter().find_map(|r| match &r.content {
        Content::Link {
            target: nt,
            rect: nrect,
            preview: Some(p),
            ..
        } if full_key(&m.car.key_format, nt) == target_key && nrect != rect => Some((
            full_key(&m.car.key_format, &r.key),
            codec::read_png(&a.join(p)).unwrap(),
        )),
        _ => None,
    });

    // Opaque edit -> the LZFSE bitmap round-trip is pixel-exact.
    let edited = checker(rect[2], rect[3], [230, 40, 20, 255], [20, 60, 220, 255]);
    codec::write_png(&a.join(preview), &edited).unwrap();

    scar::compile::compile(&a, &b).unwrap();
    common::assert_assetutil_accepts(&b);
    scar::decompile::decompile(&b, &c, false).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let link_c =
        find_rendition(&mc, &full_key(&m.car.key_format, &link.key)).expect("edited link present");
    let Content::Link {
        rect: rect_c,
        preview: Some(preview_c),
        ..
    } = &link_c.content
    else {
        panic!("edited link must still be a link rendition");
    };
    assert_eq!(rect_c, rect, "the link rect must be emitted unchanged");
    let decoded = codec::read_png(&c.join(preview_c)).unwrap();
    assert_eq!(
        decoded.rgba, edited.rgba,
        "the edited crop must come back out of the atlas exactly"
    );

    let atlas_c = find_rendition(
        &mc,
        &full_key(&m.car.key_format, &m.renditions[atlas_idx].key),
    )
    .unwrap();
    assert_eq!(
        (atlas_c.width, atlas_c.height),
        (
            m.renditions[atlas_idx].width,
            m.renditions[atlas_idx].height
        )
    );
    if let Some((nkey, npx_before)) = neighbor {
        let n_c = find_rendition(&mc, &nkey).unwrap();
        let Content::Link {
            preview: Some(np), ..
        } = &n_c.content
        else {
            panic!("neighbor still a link")
        };
        let npx_after = codec::read_png(&c.join(np)).unwrap();
        assert!(
            max_channel_diff(&npx_before, &npx_after) <= 1,
            "an unedited link into the same atlas must not change"
        );
    }

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn color_link_edit_into_a_ga8_deepmap2_atlas_promotes_the_atlas() {
    let Some(car) = sample_car() else {
        eprintln!("sample Assets.car not present, skipping");
        return;
    };
    let a = tmp("ga8-a");
    let b = tmp("ga8-b.car");
    let c = tmp("ga8-c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(car, &a, false).unwrap();
    let m = Manifest::load(&a.join("manifest.json")).unwrap();

    let Some((link_idx, atlas_idx)) = find_link(&m, |atlas| {
        atlas.pixel_format == "GA8"
            && matches!(&atlas.content, Content::RawPayload { kind, edit_hash: Some(_), .. } if kind == "celm-deepmap2")
    }) else {
        eprintln!("no editable link into a GA8 deepmap2 atlas, skipping");
        return;
    };
    let link = &m.renditions[link_idx];
    let Content::Link {
        rect,
        preview: Some(preview),
        ..
    } = &link.content
    else {
        unreachable!()
    };

    let edited = checker(rect[2], rect[3], [220, 30, 30, 255], [30, 30, 200, 255]);
    codec::write_png(&a.join(preview), &edited).unwrap();

    scar::compile::compile(&a, &b).unwrap();
    common::assert_assetutil_accepts(&b);
    scar::decompile::decompile(&b, &c, false).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let atlas_c = find_rendition(
        &mc,
        &full_key(&m.car.key_format, &m.renditions[atlas_idx].key),
    )
    .unwrap();
    assert_eq!(
        atlas_c.pixel_format, "ARGB",
        "a color paste must promote the GA8 atlas to ARGB"
    );

    let link_c = find_rendition(&mc, &full_key(&m.car.key_format, &link.key)).unwrap();
    let Content::Link {
        preview: Some(pc), ..
    } = &link_c.content
    else {
        panic!("still a link")
    };
    let decoded = codec::read_png(&c.join(pc)).unwrap();
    // The deepmap2 YCoCg-R "default" re-encode is within +/-2 per channel, hence the tolerance.
    assert!(
        max_channel_diff(&decoded, &edited) <= 2,
        "the colored crop must survive the promoted atlas re-encode"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}

/// INLK rects use a bottom-up y origin (docs/FORMAT.md §6.4). Pins crop and paste to absolute atlas
/// coordinates: round-trip tests can't catch a flipped convention because decompile and compile share the flip.
#[test]
fn multi_row_atlas_link_crop_and_paste_use_bottom_up_y() {
    use scar::manifest::Rendition as R;

    let dir = tmp("flip-dir");
    let car1 = tmp("flip-1.car");
    let out1 = tmp("flip-o1");
    let car2 = tmp("flip-2.car");
    let out2 = tmp("flip-o2");
    for d in [&dir, &out1, &out2] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(dir.join("renditions")).unwrap();

    // Every pixel unique in (r, g) = (column, top-down row); opaque so encode/decode is exact.
    let (aw, ah) = (16u32, 48u32);
    let mut atlas_rgba = Vec::with_capacity((aw * ah * 4) as usize);
    for y in 0..ah {
        for x in 0..aw {
            atlas_rgba.extend_from_slice(&[(x * 15) as u8, (y * 5) as u8, 128, 255]);
        }
    }
    let atlas_px = Pixels {
        width: aw,
        height: ah,
        rgba: atlas_rgba,
    };
    codec::write_png(&dir.join("renditions/atlas.png"), &atlas_px).unwrap();

    // Bottom-up rect: x=4, y=2, 8x8. Top-down row = 48 - 2 - 8 = 38 (!= 2).
    let rect = [4u32, 2, 8, 8];
    let y_top = ah - rect[1] - rect[3];
    assert_ne!(y_top, rect[1], "fixture must exercise a non-identity flip");

    let atlas_key: BTreeMap<String, u16> =
        [("element".to_string(), 9u16), ("part".to_string(), 181)]
            .into_iter()
            .collect();
    let base = R {
        key: BTreeMap::new(),
        name: String::new(),
        layout: 0,
        flags: 0,
        pixel_format: "ARGB".to_string(),
        color_space_id: 1,
        width: 0,
        height: 0,
        scale: 100,
        modified: 0,
        slices: None,
        metrics: None,
        composition: None,
        bitmap_info: None,
        extra_tlvs: BTreeMap::new(),
        content: Content::Multisize { sizes: vec![] }, // placeholder, replaced below
    };
    let manifest = Manifest {
        car: common::synthetic_car_info(),
        facets: vec![],
        appearances: BTreeMap::new(),
        localizations: BTreeMap::new(),
        renditions: vec![
            R {
                key: atlas_key.clone(),
                name: "atlas.png".to_string(),
                layout: 1004,
                width: aw,
                height: ah,
                content: Content::Image {
                    file: "renditions/atlas.png".to_string(),
                    compression: "lzfse".to_string(),
                    original: None,
                    edit_hash: None,
                },
                ..base.clone()
            },
            R {
                key: [("element".to_string(), 1u16), ("identifier".to_string(), 1)]
                    .into_iter()
                    .collect(),
                name: "crop.png".to_string(),
                layout: 1003,
                width: rect[2],
                height: rect[3],
                content: Content::Link {
                    target: atlas_key,
                    rect,
                    content_layout: 12,
                    preview: None,
                    edit_hash: None,
                },
                ..base
            },
        ],
        bitmap_keys: BTreeMap::new(),
    };
    manifest.save(&dir.join("manifest.json")).unwrap();

    scar::compile::compile(&dir, &car1).unwrap();
    scar::decompile::decompile(&car1, &out1, false).unwrap();
    let m1 = Manifest::load(&out1.join("manifest.json")).unwrap();
    let link = m1
        .renditions
        .iter()
        .find(|r| matches!(r.content, Content::Link { .. }))
        .unwrap();
    let Content::Link {
        preview: Some(preview),
        edit_hash: Some(_),
        ..
    } = &link.content
    else {
        panic!("link should get an editable preview");
    };
    let crop = codec::read_png(&out1.join(preview)).unwrap();
    let mut expected = Vec::new();
    for row in 0..rect[3] {
        let start = (((y_top + row) * aw + rect[0]) * 4) as usize;
        expected.extend_from_slice(&atlas_px.rgba[start..start + (rect[2] * 4) as usize]);
    }
    assert_eq!(
        crop.rgba, expected,
        "crop must come from top-down row {y_top}, not row {}",
        rect[1]
    );

    let edit = checker(rect[2], rect[3], [250, 10, 40, 255], [10, 250, 90, 255]);
    codec::write_png(&out1.join(preview), &edit).unwrap();
    scar::compile::compile(&out1, &car2).unwrap();
    scar::decompile::decompile(&car2, &out2, false).unwrap();
    let m2 = Manifest::load(&out2.join("manifest.json")).unwrap();
    let atlas2 = m2
        .renditions
        .iter()
        .find(|r| matches!(r.content, Content::Image { .. }))
        .unwrap();
    let Content::Image { file, .. } = &atlas2.content else {
        unreachable!()
    };
    let atlas_after = codec::read_png(&out2.join(file)).unwrap();
    let mut expected_atlas = atlas_px.rgba.clone();
    for row in 0..rect[3] {
        let dst = (((y_top + row) * aw + rect[0]) * 4) as usize;
        let src = (row * rect[2] * 4) as usize;
        expected_atlas[dst..dst + (rect[2] * 4) as usize]
            .copy_from_slice(&edit.rgba[src..src + (rect[2] * 4) as usize]);
    }
    assert_eq!(
        atlas_after.rgba, expected_atlas,
        "paste must land on top-down row {y_top} and leave the rest of the atlas untouched"
    );

    for d in [&dir, &out1, &out2] {
        let _ = std::fs::remove_dir_all(d);
    }
    let _ = std::fs::remove_file(&car1);
    let _ = std::fs::remove_file(&car2);
}

#[test]
fn unedited_link_previews_leave_atlases_byte_identical() {
    // Passthrough atlases only; Image atlases are covered pixel-exactly by the roundtrip suite.
    let Some(car) = sample_car() else {
        eprintln!("sample Assets.car not present, skipping");
        return;
    };
    let a = tmp("ctrl-a");
    let b = tmp("ctrl-b.car");
    let c = tmp("ctrl-c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(car, &a, false).unwrap();
    scar::compile::compile(&a, &b).unwrap();
    common::assert_assetutil_accepts(&b);
    scar::decompile::decompile(&b, &c, false).unwrap();

    let ma = Manifest::load(&a.join("manifest.json")).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let mut checked = 0;
    for r in &ma.renditions {
        let Content::Link { target, .. } = &r.content else {
            continue;
        };
        let target_key = full_key(&ma.car.key_format, target);
        let Some(atlas_a) = find_rendition(&ma, &target_key) else {
            continue;
        };
        let Content::RawPayload { file: fa, .. } = &atlas_a.content else {
            continue;
        };
        let atlas_c =
            find_rendition(&mc, &target_key).expect("link-target atlas present after round-trip");
        let Content::RawPayload { file: fc, .. } = &atlas_c.content else {
            panic!(
                "atlas {:?} changed content kind without an edit",
                atlas_a.name
            );
        };
        assert_eq!(
            std::fs::read(a.join(fa)).unwrap(),
            std::fs::read(c.join(fc)).unwrap(),
            "atlas {:?} must be byte-identical when none of its link previews changed",
            atlas_a.name
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one link-targeted passthrough atlas in the sample"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn wrong_size_link_edit_fails_with_a_clear_error() {
    let Some(car) = sample_car() else {
        eprintln!("sample Assets.car not present, skipping");
        return;
    };
    let a = tmp("err-a");
    let b = tmp("err-b.car");
    let _ = std::fs::remove_dir_all(&a);

    scar::decompile::decompile(car, &a, false).unwrap();
    let m = Manifest::load(&a.join("manifest.json")).unwrap();
    let Some((link_idx, _)) = find_link(&m, |_| true) else {
        eprintln!("no editable link, skipping");
        return;
    };
    let Content::Link {
        rect,
        preview: Some(preview),
        ..
    } = &m.renditions[link_idx].content
    else {
        unreachable!()
    };

    let wrong = checker(rect[2] + 3, rect[3] + 1, [255, 0, 0, 255], [0, 0, 255, 255]);
    codec::write_png(&a.join(preview), &wrong).unwrap();

    let err = scar::compile::compile(&a, &b).expect_err("size-mismatched link edit must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not resample"),
        "error should say scar does not resample, got: {msg}"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_file(&b);
}
