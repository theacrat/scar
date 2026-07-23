//! Oracle for deepmap_encode::encode_palette: two recompiles differing only in one rendition's
//! payload (verbatim vs re-encoded) must render identically under CoreUI. macOS only.
//!
//! Usage: cargo run --release --example validate_palette_oracle [-- <car> [target]]

use std::path::Path;
use std::process::exit;

use scar::codec::Pixels;
use scar::manifest::{Content, Manifest};

#[path = "common/util.rs"]
mod util;

const DEFAULT_CAR: &str =
    "tests/re_catalogs/CoreServices_Setup Assistant.app_Contents_Resources_Assets.c";
const DEFAULT_TARGET: &str = "selectionColor_mask-rtl.png";

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
    let work = util::workdir("palette-oracle");
    println!("workdir: {}", work.display());
    println!("catalog: {car}");
    println!("target:  {target}");

    let dec = work.join("dec");
    scar::decompile::decompile(Path::new(&car), &dec, false).expect("decompile");

    // Locate the target palette rendition (dmp2 codec 4) + its decoded preview.
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
                if d[i + 4] == 4 {
                    tgt = Some((r.width, r.height, file.clone(), preview.clone()));
                    break;
                }
            }
        }
    }
    let Some((w, h, binfile, preview)) = tgt else {
        println!("FAIL: no palette rendition named {target} with a preview in this catalog");
        exit(1);
    };
    println!("found target: {target} {w}x{h} bin={binfile}");

    let base = work.join("base.car");
    scar::compile::compile(&dec, &base).expect("compile baseline");
    let bout = work.join("bout");
    util::cuidump(&base, &bout, None);

    let px = scar::codec::read_png(&dec.join(&preview)).expect("reading preview");
    let payload = scar::deepmap_encode::encode_palette(&px)
        .expect("encode_palette err")
        .expect("encode_palette returned None");
    let orig_bin = std::fs::metadata(dec.join(&binfile)).unwrap().len();
    println!(
        "encode_palette: {w}x{h} -> {} bytes (orig bin {orig_bin})",
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

    let (worst, total, _) = util::compare_dump_dirs(&bout, &eout, 1);
    println!("BASELINE vs EDITED: {}/{total} images differ", worst.len());
    for (d, n) in worst.iter().take(6) {
        println!("  {n}: {d} channels off by >1");
    }
    if !worst.is_empty() {
        println!("FAIL: CoreUI rendered the encoded pixels differently from the original.");
        exit(1);
    }
    println!(
        "PASS (exact): assetutil accepted AND CoreUI rendered the encoded palette pixels byte-identically."
    );

    // Phase 2: a >256-colour gradient exercises median-cut quantisation.
    let mut grad = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            grad.extend_from_slice(&[
                ((x * 256) / w) as u8,
                ((y * 256) / h) as u8,
                (((x + y) * 3) & 255) as u8,
                255,
            ]);
        }
    }
    let qpx = Pixels {
        width: w,
        height: h,
        rgba: grad,
    };
    let qpayload = scar::deepmap_encode::encode_palette(&qpx)
        .expect("encode_palette err")
        .expect("encode_palette returned None");
    let qcount = (u32::from_le_bytes(qpayload[44..48].try_into().unwrap()) & 0xFFFF) + 1;
    std::fs::write(dec.join(&binfile), &qpayload).unwrap();
    let qcar = work.join("quant.car");
    scar::compile::compile(&dec, &qcar).expect("compile quantised");
    let (qcode, _) = util::assetutil(&qcar);
    let qout = work.join("qout");
    util::cuidump(&qcar, &qout, None);

    let qdec = work.join("qdec");
    scar::decompile::decompile(&qcar, &qdec, false).expect("decompile quantised");
    let qman = Manifest::load(&qdec.join("manifest.json")).unwrap();
    let qprev = qman
        .renditions
        .iter()
        .find_map(|r| match (&r.name == &target, &r.content) {
            (
                true,
                Content::RawPayload {
                    preview: Some(p), ..
                },
            ) => Some(p.clone()),
            _ => None,
        })
        .expect("quantised preview");
    let mut sp = scar::codec::read_png(&qdec.join(&qprev)).unwrap().rgba;
    util::premultiply_buf(&mut sp);

    let mut best: Option<(u64, String, Vec<u8>)> = None;
    for e in std::fs::read_dir(&qout).unwrap().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("rgbaref") {
            continue;
        }
        let (ww, hh, px) = util::read_rgbaref(&p);
        if (ww, hh) != (w, h) || px.len() != sp.len() {
            continue;
        }
        let err: u64 = px
            .iter()
            .zip(&sp)
            .map(|(a, b)| (*a as i64 - *b as i64).unsigned_abs())
            .sum();
        if best.as_ref().is_none_or(|(e0, _, _)| err < *e0) {
            best = Some((err, e.file_name().to_string_lossy().into_owned(), px));
        }
    }
    let Some((err, name, px)) = best else {
        println!("FAIL: no dims-matching CoreUI dump for the quantised render");
        exit(1);
    };
    let mx = util::max_abs_diff(&px, &sp);
    println!(
        "quantised palette count={qcount}; assetutil exit {qcode}; CoreUI vs scar decode [{name}]: mean={:.3} max={mx}",
        err as f64 / px.len() as f64
    );
    if qcode == 0 && mx <= 1 {
        println!("PASS (quantise): CoreUI decodes a full 256+1-entry palette identically to scar.");
        return;
    }
    println!("FAIL: quantised palette mismatch or assetutil rejected.");
    exit(1);
}
