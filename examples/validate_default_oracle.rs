//! Oracle for deepmap_encode::encode_default (YCoCg-R): verbatim vs re-encoded recompiles must
//! render within ±1/channel under CoreUI (premultiplied rasterisation rounding). macOS only.
//!
//! Usage: cargo run --release --example validate_default_oracle [-- <car> [target]]

use std::path::Path;
use std::process::exit;

use scar::manifest::{Content, Manifest};

#[path = "common/util.rs"]
mod util;

const DEFAULT_CAR: &str =
    "tests/re_catalogs/CoreServices_Setup Assistant.app_Contents_Resources_Assets.c";
const DEFAULT_TARGET: &str = "wallpaper-dark@1x.png";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let car = args
        .first()
        .map(String::as_str)
        .unwrap_or(DEFAULT_CAR)
        .to_string();
    let target = args
        .get(1)
        .map(String::as_str)
        .unwrap_or(DEFAULT_TARGET)
        .to_string();
    let work = util::workdir("default-oracle");
    println!("workdir: {}", work.display());
    println!("catalog: {car}");
    println!("target:  {target}");

    let dec = work.join("dec");
    scar::decompile::decompile(Path::new(&car), &dec, false).expect("decompile");

    // Locate the target Default (dmp2 codec 2) rendition, GA8 or BGRA.
    let man = Manifest::load(&dec.join("manifest.json")).unwrap();
    let mut tgt = None;
    for r in &man.renditions {
        if r.name != target {
            continue;
        }
        if let Content::RawPayload {
            kind,
            file,
            preview: Some(preview),
            ..
        } = &r.content
        {
            if kind != "celm-deepmap2" {
                continue;
            }
            let d = std::fs::read(dec.join(file)).unwrap();
            if let Some(i) = d.windows(4).position(|w| w == b"dmp2") {
                if d[i + 4] == 2 && matches!(d[i + 7], 2 | 4) {
                    tgt = Some((r.width, r.height, file.clone(), preview.clone(), d[i + 7]));
                    break;
                }
            }
        }
    }
    let Some((w, h, binfile, preview, pf_byte)) = tgt else {
        println!("FAIL: no Default (codec 2) rendition named {target} with a preview");
        exit(1);
    };
    let pf = if pf_byte == 2 {
        scar::format::pixel_format::GA8
    } else {
        scar::format::pixel_format::ARGB
    };
    let fmt = if pf_byte == 2 { "GA8" } else { "ARGB" };
    println!("found target: {target} {w}x{h} fmt={fmt} bin={binfile}");

    let base = work.join("base.car");
    scar::compile::compile(&dec, &base).expect("compile baseline");
    let bout = work.join("bout");
    util::cuidump(&base, &bout, None);

    let px = scar::codec::read_png(&dec.join(&preview)).expect("reading preview");
    let payload = scar::deepmap_encode::encode_default(&px, pf)
        .expect("encode_default err")
        .expect("encode_default returned None");
    let orig_bin = std::fs::metadata(dec.join(&binfile)).unwrap().len();
    println!(
        "encode_default: {w}x{h} -> {} bytes (orig bin {orig_bin})",
        payload.len()
    );

    std::fs::write(dec.join(&binfile), &payload).unwrap();
    let edited = work.join("edited.car");
    scar::compile::compile(&dec, &edited).expect("compile edited");
    let (code, _) = util::assetutil(&edited);
    println!("assetutil -I exit: {code}");
    if code != 0 {
        println!("FAIL: assetutil rejected the edited catalog");
        exit(1);
    }
    let eout = work.join("eout");
    util::cuidump(&edited, &eout, None);

    let (worst, total, peak) = util::compare_dump_dirs(&bout, &eout, 1);
    println!(
        "BASELINE vs EDITED: {}/{total} images have channels off by >1; peak abs delta {peak}",
        worst.len()
    );
    for (d, n) in worst.iter().take(6) {
        println!("  {n}: {d} channels off by >1");
    }
    if !worst.is_empty() {
        println!("FAIL: CoreUI rendered the encoded pixels off by more than ±1.");
        exit(1);
    }
    println!(
        "PASS (±1): assetutil accepted AND CoreUI rendered encode_default within ±1 (peak {peak})."
    );
}
