//! Hand-synthesized (no Apple-derived bytes) coverage for rendition types no public tool can produce,
//! validated by assetutil acceptance (macOS only) and a byte-identical decompile round trip.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use scar::codec::{self, Pixels};
use scar::format::compression;
use scar::manifest::{Content, GradientStopManifest, Manifest, Rendition};

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-handsynth-{}-{tag}", std::process::id()));
    d
}

fn key(pairs: &[(&str, u16)]) -> BTreeMap<String, u16> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// Fields CoreUI needs to materialize a bitmap; without them assetutil reports "couldn't materialize"/Unknown.
fn materializable(r: &mut Rendition) {
    let (w, h) = (r.width, r.height);
    r.slices = Some(vec![[0, 0, w, h]]);
    r.metrics = Some(scar::manifest::Metrics {
        edge_insets: [0, 0, 0, 0],
        image_size: (w, h),
    });
    r.composition = Some(scar::manifest::Composition {
        blend_mode: 0,
        opacity: 1.0,
    });
    r.flags = 16;
}

fn rendition(name: &str, identifier: u16, layout: u16, content: Content) -> Rendition {
    Rendition {
        key: key(&[
            ("element", 85),
            ("part", 181),
            ("identifier", identifier),
            ("scale", 1),
        ]),
        name: name.to_string(),
        layout,
        flags: 0,
        pixel_format: "none".to_string(),
        color_space_id: 0,
        width: 0,
        height: 0,
        scale: 0,
        modified: 0,
        slices: None,
        metrics: None,
        composition: None,
        bitmap_info: Some(1),
        extra_tlvs: BTreeMap::new(),
        content,
    }
}

fn opaque_pixels(w: u32, h: u32) -> Pixels {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[
                (x * 31 % 256) as u8,
                (y * 47 % 256) as u8,
                ((x + y) * 13 % 256) as u8,
                255,
            ]);
        }
    }
    Pixels {
        width: w,
        height: h,
        rgba,
    }
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

/// GA16 wire rows: interleaved premultiplied (gray u16, alpha u16) LE; gray16 = g8 * 0x101 so `>> 8` is exact.
fn ga16_raw(px: &Pixels, bytes_per_row: u32) -> Vec<u8> {
    let (w, h) = (px.width as usize, px.height as usize);
    let mut raw = vec![0u8; bytes_per_row as usize * h];
    for y in 0..h {
        for x in 0..w {
            let g = px.rgba[(y * w + x) * 4] as u16;
            let out = &mut raw[y * bytes_per_row as usize + x * 4..][..4];
            out[..2].copy_from_slice(&(g * 0x101).to_le_bytes());
            out[2..].copy_from_slice(&0xffffu16.to_le_bytes());
        }
    }
    raw
}

/// Author the full exotic-feature catalog into a fresh decompiled dir.
fn author(dir: &Path) -> Manifest {
    let _ = std::fs::remove_dir_all(dir);
    for sub in ["rawpayload", "previews", "data"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }

    let mut renditions = Vec::new();

    // Native RLE: the stale edit_hash forces compile to re-encode the preview through rle::encode.
    std::fs::write(
        dir.join("rawpayload/rle.bin"),
        b"placeholder, replaced by the re-encode",
    )
    .unwrap();
    codec::write_png(&dir.join("previews/rle.png"), &gray_pixels(16, 16)).unwrap();
    let mut rle = rendition(
        "synthrle.png",
        1,
        12,
        Content::RawPayload {
            file: "rawpayload/rle.bin".to_string(),
            kind: "celm-rle".to_string(),
            preview: Some("previews/rle.png".to_string()),
            edit_hash: Some("0000000000000000".to_string()),
        },
    );
    rle.pixel_format = "GA8".to_string();
    rle.color_space_id = 1;
    rle.width = 16;
    rle.height = 16;
    rle.scale = 100;
    materializable(&mut rle);
    rle.key.insert("localization".to_string(), 2);
    renditions.push(rle);

    // COLR (1009): two gradient stops plus a system-color reference (the trailing RLOC record).
    for (name, id, components, system) in [
        ("synth/stop0", 2, vec![0.10, 0.55, 0.95, 1.0], None),
        ("synth/stop1", 3, vec![0.95, 0.30, 0.10, 1.0], None),
        (
            "synth/linkish",
            4,
            vec![0.0, 0.408, 0.855, 1.0],
            Some("linkColor"),
        ),
    ] {
        let mut c = rendition(
            name,
            id,
            1009,
            Content::Color {
                color_space: if system.is_some() { 257 } else { 1 },
                components,
                system_color: system.map(str::to_string),
                extra: String::new(),
            },
        );
        c.key = key(&[
            ("element", 85),
            ("part", 217),
            ("identifier", id),
            ("scale", 1),
        ]);
        renditions.push(c);
    }

    let mut grad = rendition(
        "synth/gradient",
        5,
        1021,
        Content::Gradient {
            gradient_type: 1,
            reserved: 0,
            start: [0.5, 0.0],
            end: [0.5, 1.0],
            stops: vec![
                GradientStopManifest {
                    location: 0.0,
                    color_name: "synth/stop0".to_string(),
                },
                GradientStopManifest {
                    location: 1.0,
                    color_name: "synth/stop1".to_string(),
                },
            ],
        },
    );
    grad.key = key(&[
        ("element", 85),
        ("part", 247),
        ("identifier", 5),
        ("scale", 1),
    ]);
    renditions.push(grad);

    // WBGR (premultiplied RGBA16F) wide-gamut bitmap.
    let wide_px = opaque_pixels(8, 8);
    let wide_raw = scar::widegamut::rgba_to_wbgr_raw(&wide_px, 8 * 8);
    std::fs::write(
        dir.join("rawpayload/wide.bin"),
        codec::celm_encode(&wide_raw, 8 * 8, compression::LZFSE).unwrap(),
    )
    .unwrap();
    let mut wide = rendition(
        "wide.png",
        6,
        12,
        Content::RawPayload {
            file: "rawpayload/wide.bin".to_string(),
            kind: "celm-lzfse-0x52474257".to_string(),
            preview: None,
            edit_hash: None,
        },
    );
    wide.pixel_format = "0x52474257".to_string();
    wide.color_space_id = 4;
    wide.width = 8;
    wide.height = 8;
    wide.scale = 100;
    materializable(&mut wide);
    renditions.push(wide);

    // GA16 (16-bit gray + alpha) wide bitmap.
    let ga16_px = gray_pixels(8, 8);
    std::fs::write(
        dir.join("rawpayload/ga16.bin"),
        codec::celm_encode(&ga16_raw(&ga16_px, 8 * 4), 8 * 4, compression::LZFSE).unwrap(),
    )
    .unwrap();
    let mut ga16 = rendition(
        "ga16.png",
        7,
        12,
        Content::RawPayload {
            file: "rawpayload/ga16.bin".to_string(),
            kind: "celm-lzfse-0x47413136".to_string(),
            preview: None,
            edit_hash: None,
        },
    );
    ga16.pixel_format = "0x47413136".to_string();
    ga16.color_space_id = 1;
    ga16.width = 8;
    ga16.height = 8;
    ga16.scale = 100;
    materializable(&mut ga16);
    renditions.push(ga16);

    // Depth gradient (1008) declares pf=ARGB but strides 8 B/px; the 0x3ef TLV (16 px * 8 = 0x80) is authoritative.
    let depth_raw: Vec<u8> = (0..(128 * 4)).map(|i| (i % 251) as u8).collect();
    std::fs::write(
        dir.join("rawpayload/depth.bin"),
        codec::celm_encode(&depth_raw, 128, compression::LZFSE).unwrap(),
    )
    .unwrap();
    let mut depth = rendition(
        "depthgrad.png",
        8,
        1008,
        Content::RawPayload {
            file: "rawpayload/depth.bin".to_string(),
            kind: "celm-lzfse-ARGB".to_string(),
            preview: None,
            edit_hash: None,
        },
    );
    depth.pixel_format = "ARGB".to_string();
    depth.width = 16;
    depth.height = 4;
    depth.scale = 100;
    depth
        .extra_tlvs
        .insert("0x3ef".to_string(), "gAAAAA==".to_string()); // u32 LE 0x80
    renditions.push(depth);

    // RTXT (layout 1007): opaque passthrough payload.
    let mut radar_payload = b"RTXT".to_vec();
    radar_payload.extend_from_slice(&[0u8; 12]);
    std::fs::write(dir.join("rawpayload/radar.bin"), &radar_payload).unwrap();
    let mut radar = rendition(
        "SynthRadar",
        9,
        1007,
        Content::RawPayload {
            file: "rawpayload/radar.bin".to_string(),
            kind: "unknown".to_string(),
            preview: None,
            edit_hash: None,
        },
    );
    radar.pixel_format = "ARGB".to_string();
    radar.color_space_id = 1;
    radar.width = 16;
    radar.height = 4;
    // 0x3f6 as carried by real layout-1007 renditions: three u32 LE {0, 2, 1}.
    radar
        .extra_tlvs
        .insert("0x3f6".to_string(), "AAAAAAIAAAABAAAA".to_string());
    renditions.push(radar);

    // Vector-glyph layout 1017 (SVG data rendition).
    std::fs::write(
        dir.join("data/glyph.svg"),
        "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"#123456\"/></svg>",
    )
    .unwrap();
    let mut vg = rendition(
        "glyph1017.svg",
        10,
        1017,
        Content::Data {
            file: "data/glyph.svg".to_string(),
            lzfse: false,
        },
    );
    vg.pixel_format = "SVG".to_string();
    vg.scale = 100;
    renditions.push(vg);

    // Iconstack layouts 1019 (layered image) and 1020 (icon group).
    std::fs::write(
        dir.join("data/stack.bin"),
        b"synthetic layered-image placeholder",
    )
    .unwrap();
    let mut stack = rendition(
        "SynthStack",
        11,
        1019,
        Content::Data {
            file: "data/stack.bin".to_string(),
            lzfse: false,
        },
    );
    stack.pixel_format = "DATA".to_string();
    stack.width = 64;
    stack.height = 64;
    stack.scale = 100;
    // UTI TLV: u32 LE length 0x14, u32 0, "public.layeredimage\0".
    stack.extra_tlvs.insert(
        "0x3ed".to_string(),
        "FAAAAAAAAABwdWJsaWMubGF5ZXJlZGltYWdlAA==".to_string(),
    );
    renditions.push(stack);

    std::fs::write(
        dir.join("data/group.bin"),
        b"synthetic icon-group placeholder",
    )
    .unwrap();
    let mut group = rendition(
        "SynthGroup",
        12,
        1020,
        Content::Data {
            file: "data/group.bin".to_string(),
            lzfse: false,
        },
    );
    group.pixel_format = "DATA".to_string();
    group.scale = 100;
    renditions.push(group);

    // One facet per asset: facet-less bitmaps show up as unmaterializable "Unknown" in assetutil -I.
    let facets = renditions
        .iter()
        .map(|r| scar::manifest::Facet {
            name: r.name.clone(),
            hotspot: Some((0, 0)),
            attributes: r
                .key
                .iter()
                .filter(|(k, _)| matches!(k.as_str(), "element" | "part" | "identifier"))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        })
        .collect();

    let manifest = Manifest {
        car: common::synthetic_car_info(),
        facets,
        appearances: BTreeMap::new(),
        localizations: [("en".to_string(), 2u16)].into_iter().collect(),
        renditions,
        bitmap_keys: BTreeMap::new(),
    };
    manifest.save(&dir.join("manifest.json")).unwrap();
    manifest
}

#[test]
fn hand_synth_catalog_round_trips_and_apple_accepts_it() {
    let authored = tmp("authored");
    let car1 = tmp("1.car");
    let a = tmp("a");
    let car2 = tmp("2.car");
    let c = tmp("c");
    for d in [&a, &c] {
        let _ = std::fs::remove_dir_all(d);
    }
    author(&authored);

    scar::compile::compile(&authored, &car1).expect("compiling the hand-authored catalog");
    common::assert_assetutil_accepts(&car1);
    assert_assetutil_census(&car1);
    assert!(
        bom_var_names(&car1).contains(&"LOCALIZATIONKEYS".to_string()),
        "manifest.localizations must materialize the LOCALIZATIONKEYS BOM variable"
    );

    // First decompile normalizes the authored manifest; from there the round trip must be a fixpoint.
    scar::decompile::decompile(&car1, &a, false).expect("decompiling the compiled catalog");
    scar::compile::compile(&a, &car2).expect("recompiling");
    common::assert_assetutil_accepts(&car2);
    scar::decompile::decompile(&car2, &c, false).expect("re-decompiling");
    assert!(
        dirs_identical(&a, &c),
        "hand-synth round trip is not byte-identical"
    );

    let m = Manifest::load(&a.join("manifest.json")).unwrap();
    let by_name = |n: &str| {
        m.renditions
            .iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("rendition {n} missing after round trip"))
    };

    let rle = by_name("synthrle.png");
    assert_eq!(rle.pixel_format, "GA8");
    let Content::RawPayload {
        kind,
        preview: Some(preview),
        ..
    } = &rle.content
    else {
        panic!(
            "RLE rendition should decompile as a celm-rle raw payload, got {:?}",
            rle.content
        );
    };
    assert_eq!(kind, "celm-rle");
    let decoded = codec::read_png(&a.join(preview)).unwrap();
    assert_eq!(
        decoded.rgba,
        gray_pixels(16, 16).rgba,
        "RLE decode must recover the encoded pixels"
    );

    let Content::Gradient {
        stops,
        gradient_type,
        ..
    } = &by_name("synth/gradient").content
    else {
        panic!("gradient content lost");
    };
    assert_eq!(*gradient_type, 1);
    assert_eq!(
        stops
            .iter()
            .map(|s| s.color_name.as_str())
            .collect::<Vec<_>>(),
        ["synth/stop0", "synth/stop1"]
    );

    let Content::Color { system_color, .. } = &by_name("synth/linkish").content else {
        panic!("system color content lost");
    };
    assert_eq!(system_color.as_deref(), Some("linkColor"));

    for (name, want) in [
        ("wide.png", opaque_pixels(8, 8)),
        ("ga16.png", gray_pixels(8, 8)),
    ] {
        let Content::RawPayload {
            preview: Some(preview),
            ..
        } = &by_name(name).content
        else {
            panic!("{name}: wide rendition must decompile with a decoded preview");
        };
        let got = codec::read_png(&a.join(preview)).unwrap();
        assert_eq!(
            got.rgba, want.rgba,
            "{name}: wide decode must invert the hand-built encoding"
        );
    }

    // If layout 1008 ever decodes as a 4 B/px Image it will corrupt on re-encode.
    assert!(
        matches!(by_name("depthgrad.png").content, Content::RawPayload { .. }),
        "layout-1008 stride guard must keep the depth gradient passthrough"
    );

    if std::env::var_os("SCAR_HANDSYNTH_KEEP").is_none() {
        for d in [&authored, &a, &c] {
            let _ = std::fs::remove_dir_all(d);
        }
        for f in [&car1, &car2] {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// CoreUI must recognize (and for bitmaps, materialize) every hand-synth rendition type; skips off-macOS.
fn assert_assetutil_census(car: &Path) {
    if !cfg!(target_os = "macos") || !Path::new("/usr/bin/assetutil").exists() {
        return;
    }
    let out = std::process::Command::new("/usr/bin/assetutil")
        .arg("-I")
        .arg(car)
        .output()
        .expect("running assetutil");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("couldn't materialize"),
        "every hand-synth rendition should materialize, got:\n{stderr}"
    );
    for needle in [
        "\"Named Gradient\"",    // ARGG
        "\"Texture Rendition\"", // RTXT
        "\"IconImageStack\"",    // layout 1019
        "\"IconGroup\"",         // layout 1020
        "\"Vector Glyph\"",      // layout 1017
        "\"Compression\" : \"rle\"",
        "synth\\/linkish", // the system-color rendition
    ] {
        assert!(
            stdout.contains(needle),
            "assetutil census is missing {needle}\n{stdout}"
        );
    }
}

/// Sorted top-level BOM variable names of a .car file.
fn bom_var_names(car: &Path) -> Vec<String> {
    let data = std::fs::read(car).unwrap();
    let vars_off = u32::from_be_bytes(data[24..28].try_into().unwrap()) as usize;
    let count = u32::from_be_bytes(data[vars_off..vars_off + 4].try_into().unwrap()) as usize;
    let mut p = vars_off + 4;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let name_len = data[p + 4] as usize;
        p += 5;
        names.push(String::from_utf8_lossy(&data[p..p + name_len]).into_owned());
        p += name_len;
    }
    names.sort();
    names
}

fn dirs_identical(a: &Path, b: &Path) -> bool {
    let mut fa = list_files(a);
    let mut fb = list_files(b);
    fa.sort();
    fb.sort();
    if fa != fb {
        return false;
    }
    fa.iter()
        .all(|rel| std::fs::read(a.join(rel)).ok() == std::fs::read(b.join(rel)).ok())
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    out
}
