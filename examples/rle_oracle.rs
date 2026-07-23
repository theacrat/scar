//! Oracle for src/rle.rs: CoreUI's own renderer must decode a recompiled RLE edit byte-exact
//! to the edited pixels (assetutil must also accept the catalog with an unchanged census).
//! Needs a catalog whose celm-rle rendition belongs to a NAMED facet (CUICatalog resolves by name).
//!
//! Usage: cargo run --release --example rle_oracle -- <path/to/Assets.car>

use std::path::Path;
use std::process::exit;

use scar::manifest::{Content, Manifest};

#[path = "common/util.rs"]
mod util;

fn main() {
    let Some(car) = std::env::args().nth(1) else {
        eprintln!("usage: cargo run --release --example rle_oracle -- <path/to/Assets.car>");
        exit(2);
    };
    let car = Path::new(&car);
    let work = util::workdir("rle-oracle");
    println!("== workdir: {}", work.display());

    println!("== decompiling {}", car.display());
    let dec = work.join("decompiled");
    scar::decompile::decompile(car, &dec, false).expect("decompile");

    let man = Manifest::load(&dec.join("manifest.json")).unwrap();
    let Some((rend, preview)) = man.renditions.iter().find_map(|r| match &r.content {
        Content::RawPayload {
            kind,
            preview: Some(p),
            ..
        } if kind == "celm-rle" => Some((r, p.clone())),
        _ => None,
    }) else {
        eprintln!(
            "no celm-rle rendition found in {} -- nothing to validate",
            car.display()
        );
        exit(1);
    };
    let facet = man
        .facets
        .iter()
        .find(|f| f.attributes.iter().all(|(k, v)| rend.key.get(k) == Some(v)))
        .map(|f| f.name.clone());
    let Some(facet) = facet else {
        eprintln!(
            "celm-rle rendition has no named facet (packed asset?) -- cannot resolve via CUICatalog"
        );
        exit(1);
    };
    let (rw, rh, rscale) = (rend.width, rend.height, rend.scale.max(100));
    println!(
        "== RLE rendition: preview={preview} facet={facet} size={rw}x{rh}@{}x",
        rscale / 100
    );

    // Edit exercises fill runs, a literal run, and a fully transparent (dedup-eligible) row.
    let mut px = scar::codec::read_png(&dec.join(&preview)).expect("reading preview");
    let (w, h) = (px.width as usize, px.height as usize);
    let transparent_row = h / 2;
    for y in 0..h {
        for x in 0..w {
            let out = &mut px.rgba[(y * w + x) * 4..][..4];
            if y == transparent_row {
                out.copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let gray = if x < w / 3 {
                200 // long fill run
            } else if x < 2 * w / 3 {
                ((x * 5) % 256) as u8 // mostly-distinct literal run
            } else {
                50 // another fill run
            };
            out.copy_from_slice(&[gray, gray, gray, 255]);
        }
    }
    scar::codec::write_png(&dec.join(&preview), &px).unwrap();
    println!("edited {preview} ({w}x{h}), forced row {transparent_row} transparent");

    println!("== compiling edited catalog");
    let edited = work.join("edited.car");
    scar::compile::compile(&dec, &edited).expect("compile");

    println!("== assetutil -I (structural acceptance)");
    if Path::new("/usr/bin/assetutil").exists() {
        let (ocode, ojson) = util::assetutil(car);
        let (ecode, ejson) = util::assetutil(&edited);
        if ecode != 0 {
            eprintln!("FAIL: assetutil -I rejected the recompiled catalog");
            exit(1);
        }
        let count = |s: &str| {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| v.as_array().map(Vec::len))
                .unwrap_or(0)
        };
        let (no, ne) = (count(&ojson), count(&ejson));
        println!("   original entries: {no}, edited entries: {ne}");
        if ocode == 0 && no != ne {
            eprintln!("FAIL: rendition census changed ({no} -> {ne})");
            exit(1);
        }
        if let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(&ejson) {
            if let Some(e) = entries.iter().find(|e| e["Compression"] == "rle") {
                println!(
                    "   RLE entry in recompiled catalog: {} {} {} {}",
                    e["Name"], e["Encoding"], e["Idiom"], e["SizeOnDisk"]
                );
            }
        }
    } else {
        eprintln!("assetutil not found -- skipping structural check (macOS only)");
    }

    println!("== dumping CoreUI's own decode of the edited catalog");
    let dump = work.join("dump");
    util::cuidump(&edited, &dump, Some(&facet));

    println!("== comparing against expected pixels");
    let mut expected = px.rgba.clone();
    util::premultiply_buf(&mut expected);
    let safe: String = facet
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let prefix = format!("{safe}__{w}x{h}@{}x__", rscale / 100);
    let mut candidates: Vec<_> = std::fs::read_dir(&dump)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().into_owned();
            n.starts_with(&prefix) && n.ends_with(".rgbaref")
        })
        .collect();
    candidates.sort();
    if candidates.is_empty() {
        eprintln!(
            "FAIL: no dumped .rgbaref matched facet={facet} size={w}x{h}@{}x",
            rscale / 100
        );
        exit(1);
    }
    let mut best: Option<(usize, String)> = None;
    for c in &candidates {
        let (_, _, refpx) = util::read_rgbaref(c);
        let diffs = expected.iter().zip(&refpx).filter(|(a, b)| a != b).count()
            + expected.len().abs_diff(refpx.len());
        let name = c.file_name().unwrap().to_string_lossy().into_owned();
        if best.as_ref().is_none_or(|(d, _)| diffs < *d) {
            best = Some((diffs, name));
        }
    }
    let (best_diffs, best_name) = best.unwrap();
    println!(
        "best match: {best_name} - {best_diffs}/{} bytes differ",
        expected.len()
    );
    if best_diffs == 0 {
        println!("PASS: CoreUI decoded the custom RLE payload byte-exact to the edited pixels");
    } else {
        eprintln!("FAIL: CoreUI's decoded pixels do not match the edit");
        exit(1);
    }
    println!("== OK: {} (kept for inspection)", work.display());
}
