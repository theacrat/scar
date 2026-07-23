//! `scar clone-asset`: duplicate a facet + renditions under a new name/identifier,
//! optionally installing a replacement image. The app-icon test needs the sample Assets.car.

mod common;

use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::manifest::{Content, Manifest};

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-clone-{}-{tag}", std::process::id()));
    d
}

fn solid(w: u32, h: u32, color: [u8; 4]) -> Pixels {
    Pixels {
        width: w,
        height: h,
        rgba: color.repeat((w * h) as usize),
    }
}

#[test]
fn clone_asset_duplicates_facet_and_renditions_with_fresh_identifier() {
    let input = tmp("syn-in");
    let packed = tmp("syn-packed");
    let car1 = tmp("syn-1.car");
    let a = tmp("syn-a");
    let car2 = tmp("syn-2.car");
    let c = tmp("syn-c");
    for d in [&input, &packed, &a, &c] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("logo.png"), &solid(24, 24, [10, 120, 240, 255])).unwrap();
    codec::write_png(&input.join("other.png"), &solid(8, 8, [128, 128, 128, 255])).unwrap();

    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();
    scar::compile::compile(&packed, &car1).unwrap();
    scar::decompile::decompile(&car1, &a, false).unwrap();

    let replacement_path = tmp("syn-replacement.png");
    let replacement = solid(24, 24, [250, 30, 60, 255]);
    codec::write_png(&replacement_path, &replacement).unwrap();
    scar::authoring::clone_asset(&a, "logo", "logo2", Some(&replacement_path)).unwrap();

    scar::compile::compile(&a, &car2).unwrap();
    common::assert_assetutil_accepts(&car2);
    scar::decompile::decompile(&car2, &c, false).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let src_facet = mc
        .facets
        .iter()
        .find(|f| f.name == "logo")
        .expect("source facet survives");
    let new_facet = mc
        .facets
        .iter()
        .find(|f| f.name == "logo2")
        .expect("cloned facet present after round-trip");
    let src_id = src_facet.attributes["identifier"];
    let new_id = new_facet.attributes["identifier"];
    assert_ne!(src_id, new_id, "the clone must get a fresh identifier");

    let src_rends: Vec<_> = mc
        .renditions
        .iter()
        .filter(|r| r.key.get("identifier") == Some(&src_id))
        .collect();
    let new_rends: Vec<_> = mc
        .renditions
        .iter()
        .filter(|r| r.key.get("identifier") == Some(&new_id))
        .collect();
    assert_eq!(
        src_rends.len(),
        new_rends.len(),
        "the clone must have one rendition per source rendition"
    );

    let Content::Image { file, .. } = &new_rends[0].content else {
        panic!("clone should be an image rendition")
    };
    assert_eq!(
        codec::read_png(&c.join(file)).unwrap().rgba,
        replacement.rgba,
        "clone must decode to --image pixels"
    );
    let Content::Image { file, .. } = &src_rends[0].content else {
        panic!("source should be an image rendition")
    };
    assert_eq!(
        codec::read_png(&c.join(file)).unwrap().rgba,
        solid(24, 24, [10, 120, 240, 255]).rgba,
        "the source asset must be untouched"
    );

    for d in [&input, &packed, &a, &c] {
        let _ = std::fs::remove_dir_all(d);
    }
    for f in [&car1, &car2, &replacement_path] {
        let _ = std::fs::remove_file(f);
    }
}

#[test]
fn clone_asset_rejects_an_image_matching_no_rendition_size() {
    let input = tmp("rej-in");
    let packed = tmp("rej-packed");
    let car1 = tmp("rej-1.car");
    let a = tmp("rej-a");
    for d in [&input, &packed, &a] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(&input).unwrap();
    codec::write_png(&input.join("logo.png"), &solid(24, 24, [10, 120, 240, 255])).unwrap();
    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();
    scar::compile::compile(&packed, &car1).unwrap();
    scar::decompile::decompile(&car1, &a, false).unwrap();

    let wrong_path = tmp("rej-wrong.png");
    codec::write_png(&wrong_path, &solid(10, 10, [1, 2, 3, 255])).unwrap();
    let err = scar::authoring::clone_asset(&a, "logo", "logo2", Some(&wrong_path))
        .expect_err("size mismatch");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not resample"),
        "error should mention resampling, got: {msg}"
    );
    assert!(
        msg.contains("24x24"),
        "error should list the sizes the asset does have, got: {msg}"
    );
    let m = Manifest::load(&a.join("manifest.json")).unwrap();
    assert!(m.facets.iter().all(|f| f.name != "logo2"));

    for d in [&input, &packed, &a] {
        let _ = std::fs::remove_dir_all(d);
    }
    let _ = std::fs::remove_file(&car1);
    let _ = std::fs::remove_file(&wrong_path);
}

#[test]
fn clone_asset_installs_image_per_size_and_keeps_other_sizes() {
    // --image matches only the 48px rendition; the 24px clone keeps the source art instead of aborting.
    let input = tmp("multi-in");
    let packed = tmp("multi-packed");
    let car1 = tmp("multi-1.car");
    let a = tmp("multi-a");
    let car2 = tmp("multi-2.car");
    let c = tmp("multi-c");
    for d in [&input, &packed, &a, &c] {
        let _ = std::fs::remove_dir_all(d);
    }
    std::fs::create_dir_all(&input).unwrap();
    let base = solid(24, 24, [10, 120, 240, 255]);
    codec::write_png(&input.join("logo.png"), &base).unwrap();
    codec::write_png(
        &input.join("logo@2x.png"),
        &solid(48, 48, [10, 120, 240, 255]),
    )
    .unwrap();

    scar::authoring::pack(&input, &packed, &scar::authoring::PackOptions::default()).unwrap();
    scar::compile::compile(&packed, &car1).unwrap();
    scar::decompile::decompile(&car1, &a, false).unwrap();

    let replacement_path = tmp("multi-replacement.png");
    let replacement = solid(48, 48, [250, 30, 60, 255]);
    codec::write_png(&replacement_path, &replacement).unwrap();
    scar::authoring::clone_asset(&a, "logo", "logo2", Some(&replacement_path)).unwrap();

    scar::compile::compile(&a, &car2).unwrap();
    common::assert_assetutil_accepts(&car2);
    scar::decompile::decompile(&car2, &c, false).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let new_id = mc
        .facets
        .iter()
        .find(|f| f.name == "logo2")
        .expect("cloned facet")
        .attributes["identifier"];
    let clone_at = |w: u32| {
        mc.renditions
            .iter()
            .find(|r| r.key.get("identifier") == Some(&new_id) && r.width == w)
            .unwrap_or_else(|| panic!("{w}px clone rendition present"))
    };
    let Content::Image { file, .. } = &clone_at(48).content else {
        panic!("48px clone should be an image")
    };
    assert_eq!(
        codec::read_png(&c.join(file)).unwrap().rgba,
        replacement.rgba,
        "matching size gets the --image pixels"
    );
    let Content::Image { file, .. } = &clone_at(24).content else {
        panic!("24px clone should be an image")
    };
    assert_eq!(
        codec::read_png(&c.join(file)).unwrap().rgba,
        base.rgba,
        "other sizes keep the cloned source art"
    );

    for d in [&input, &packed, &a, &c] {
        let _ = std::fs::remove_dir_all(d);
    }
    for f in [&car1, &car2, &replacement_path] {
        let _ = std::fs::remove_file(f);
    }
}

#[test]
fn clone_asset_clones_an_app_icon_from_the_sample_catalog() {
    let car = Path::new("/Users/thea/Downloads/Assets.car");
    if !car.exists() {
        eprintln!("sample Assets.car not present, skipping");
        return;
    }
    let a = tmp("icon-a");
    let b = tmp("icon-b.car");
    let c = tmp("icon-c");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);

    scar::decompile::decompile(car, &a, false).unwrap();
    let m = Manifest::load(&a.join("manifest.json")).unwrap();

    // Catalog-agnostic: a plain Image plus a Multisize stub is the app-icon shape.
    let picked = m.facets.iter().find_map(|f| {
        let id = *f.attributes.get("identifier")?;
        let rends: Vec<_> = m
            .renditions
            .iter()
            .filter(|r| r.key.get("identifier") == Some(&id))
            .collect();
        let image = rends
            .iter()
            .find(|r| matches!(r.content, Content::Image { .. }))?;
        rends
            .iter()
            .any(|r| matches!(r.content, Content::Multisize { .. }))
            .then(|| (f.name.clone(), id, rends.len(), (image.width, image.height)))
    });
    let Some((from, src_id, src_count, (iw, ih))) = picked else {
        eprintln!("no icon-shaped facet in sample, skipping");
        return;
    };

    let image_path = tmp("icon-art.png");
    let art = solid(iw, ih, [240, 90, 20, 255]);
    codec::write_png(&image_path, &art).unwrap();
    scar::authoring::clone_asset(&a, &from, "ScarTestIcon", Some(&image_path)).unwrap();

    scar::compile::compile(&a, &b).unwrap();
    common::assert_assetutil_accepts(&b);
    scar::decompile::decompile(&b, &c, false).unwrap();
    let mc = Manifest::load(&c.join("manifest.json")).unwrap();

    let clone_facet = mc
        .facets
        .iter()
        .find(|f| f.name == "ScarTestIcon")
        .expect("cloned facet survives compile");
    let new_id = clone_facet.attributes["identifier"];
    assert_ne!(new_id, src_id);
    let clone_rends: Vec<_> = mc
        .renditions
        .iter()
        .filter(|r| r.key.get("identifier") == Some(&new_id))
        .collect();
    assert_eq!(
        clone_rends.len(),
        src_count,
        "every source rendition must be cloned"
    );
    assert!(
        clone_rends
            .iter()
            .any(|r| matches!(r.content, Content::Multisize { .. })),
        "multisize stubs must be cloned"
    );

    let img = clone_rends
        .iter()
        .find(|r| matches!(r.content, Content::Image { .. }) && (r.width, r.height) == (iw, ih))
        .expect("cloned image rendition present");
    let Content::Image { file, .. } = &img.content else {
        unreachable!()
    };
    let decoded = codec::read_png(&c.join(file)).unwrap();
    assert_eq!(
        decoded.rgba, art.rgba,
        "the cloned icon must decode to the installed art"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&c);
    let _ = std::fs::remove_file(&b);
    let _ = std::fs::remove_file(&image_path);
}
