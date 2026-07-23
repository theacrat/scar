//! Extract real CSI rendition blobs from an Assets.car into tests/fixtures/ for the unit tests.
//!
//! Usage:
//!   cargo run --example extract_fixtures [-- <path/to/Assets.car>]   (default: the committed Setup Assistant car)
//!   cargo run --example extract_fixtures -- --single <car> <index> <name>   (one rendition by RENDITIONS index)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "common/util.rs"]
mod util;

const DEFAULT_CAR: &str =
    "tests/re_catalogs/CoreServices_Setup Assistant.app_Contents_Resources_Assets.c";
const WIDE_PIXFMTS: [u32; 2] = [0x52474257, 0x47413136]; // WBGR, GA16

struct Rend {
    val: Vec<u8>,
    kind: String,
    name: Vec<u8>,
    width: u32,
    pixfmt: u32,
}

fn classify(csi: &scar::csi::Csi) -> String {
    let p = &csi.payload;
    if p.len() >= 12 && &p[0..4] == b"MLEC" {
        let flags = u32::from_le_bytes(p[4..8].try_into().unwrap());
        let comp = u32::from_le_bytes(p[8..12].try_into().unwrap());
        let chunked = if flags & 1 != 0 { "-chunked" } else { "" };
        return format!("celm-comp{comp}{chunked}");
    }
    if p.len() >= 4 && &p[0..4] == b"DWAR" {
        return "rawd".into();
    }
    if p.len() >= 4 && &p[0..4] == b"SISM" {
        return "msis".into();
    }
    if csi.header.layout == 1003 {
        return "inlk".into();
    }
    // Mirrors the python's `other-{magic4!r}` naming (e.g. "other-b'RLOC'").
    let magic = p.get(0..4).unwrap_or(b"");
    let printable: String = magic
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) && b != b'\'' && b != b'\\' {
                (b as char).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect();
    format!("other-b'{printable}'")
}

fn renditions(car: &Path) -> Vec<Rend> {
    let data = std::fs::read(car).unwrap_or_else(|e| panic!("{}: {e}", car.display()));
    let bom = scar::bom::Bom::parse(&data).expect("parsing BOM");
    bom.tree_entries("RENDITIONS")
        .expect("RENDITIONS tree")
        .into_iter()
        .map(|(_k, val)| {
            let csi = scar::csi::Csi::parse(&val).expect("parsing CSI");
            Rend {
                kind: classify(&csi),
                name: csi.header.name.clone(),
                width: csi.header.width,
                pixfmt: csi.header.pixel_format,
                val,
            }
        })
        .collect()
}

/// Exact match, or prefix followed by a non-digit ("celm-comp1" must not match "celm-comp11").
fn kind_matches(kind: &str, prefix: &str) -> bool {
    kind == prefix || (kind.starts_with(prefix) && !kind.as_bytes()[prefix.len()].is_ascii_digit())
}

/// Candidate indices for any of `prefixes`, smallest value first, capped at n.
fn pick_smallest(rends: &[Rend], prefixes: &[&str], n: usize) -> Vec<usize> {
    let mut cands: Vec<(usize, usize)> = rends
        .iter()
        .enumerate()
        .filter(|(_, r)| prefixes.iter().any(|p| kind_matches(&r.kind, p)))
        .map(|(i, r)| (r.val.len(), i))
        .collect();
    cands.sort();
    cands.into_iter().take(n).map(|(_, i)| i).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out_dir = util::root().join("tests/fixtures");
    std::fs::create_dir_all(&out_dir).unwrap();

    if args.first().map(String::as_str) == Some("--single") {
        let [car, idx, name] = &args[1..4] else {
            panic!("--single <car> <index> <name>")
        };
        let rends = renditions(Path::new(car));
        let idx: usize = idx.parse().expect("index");
        let out = out_dir.join(format!("{name}.csi.bin"));
        std::fs::write(&out, &rends[idx].val).unwrap();
        println!("wrote {} ({} bytes)", out.display(), rends[idx].val.len());
        return;
    }

    let car = PathBuf::from(args.first().map(String::as_str).unwrap_or(DEFAULT_CAR));
    let rends = renditions(&car);

    // One index may legitimately be chosen under several names (e.g. smallest comp11 == smallest ARGB comp11).
    let mut chosen: Vec<(String, usize)> = Vec::new();

    // Prefer a small 8-bit chunked CELM: the unit test's raw_to_rgba has no wide-format support.
    let chunk_cands = pick_smallest(&rends, &["celm-comp4-chunked"], 20);
    let chunk_pick = chunk_cands
        .iter()
        .copied()
        .find(|&i| rends[i].width <= 400 && !WIDE_PIXFMTS.contains(&rends[i].pixfmt))
        .or(chunk_cands.first().copied());
    if let Some(i) = chunk_pick {
        chosen.push((format!("celm_lzfse_chunked_{i}"), i));
    }

    if let Some(&i) = pick_smallest(&rends, &["celm-comp11"], 1).first() {
        chosen.push((format!("celm_deepmap2_{i}"), i));
    }
    if let Some(&i) = pick_smallest(&rends, &["celm-comp1"], 1).first() {
        chosen.push((format!("celm_rle_{i}"), i));
    }
    // SVG only: RAWD also wraps PDFs, and the unit test asserts SVG text.
    if let Some(i) = pick_smallest(&rends, &["rawd"], 50)
        .into_iter()
        .find(|&i| rends[i].name.ends_with(b".svg"))
    {
        chosen.push((format!("rawd_svg_{i}"), i));
    }
    if let Some(&i) = pick_smallest(&rends, &["msis"], 1).first() {
        chosen.push((format!("msis_{i}"), i));
    }
    if let Some(&i) = pick_smallest(&rends, &["inlk"], 1).first() {
        chosen.push((format!("inlk_{i}"), i));
    }

    const ARGB: u32 = u32::from_le_bytes(*b"BGRA");
    let argb = |comp: u32, need_chunked: bool, label: &str, chosen: &mut Vec<(String, usize)>| {
        let mut cands: Vec<(usize, usize)> = rends
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                if r.pixfmt != ARGB {
                    return false;
                }
                let p = &scar::csi::Csi::parse(&r.val).unwrap().payload;
                if p.len() < 12 || &p[0..4] != b"MLEC" {
                    return false;
                }
                let flags = u32::from_le_bytes(p[4..8].try_into().unwrap());
                let c = u32::from_le_bytes(p[8..12].try_into().unwrap());
                c == comp && (!need_chunked || flags & 1 != 0)
            })
            .map(|(i, r)| (r.val.len(), i))
            .collect();
        cands.sort();
        if let Some(&(_, i)) = cands.first() {
            chosen.push((format!("{label}_{i}"), i));
        }
    };
    argb(11, false, "celm_argb_deepmap2", &mut chosen);
    argb(4, true, "celm_argb_lzfse_chunked", &mut chosen);

    // 10 assorted others; non-SVG RAWD excluded (the rawd unit test asserts SVG content on every rawd fixture).
    let mut pool: Vec<(usize, usize, &str)> = rends
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            !chosen.iter().any(|(_, j)| j == i) && !(r.kind == "rawd" && !r.name.ends_with(b".svg"))
        })
        .map(|(i, r)| (r.val.len(), i, r.kind.as_str()))
        .collect();
    pool.sort();
    let mut kinds_taken: BTreeMap<&str, ()> = BTreeMap::new();
    let mut assorted = Vec::new();
    for &(size, i, kind) in &pool {
        if !kinds_taken.contains_key(kind) || assorted.len() < 5 {
            assorted.push((size, i, kind));
            kinds_taken.insert(kind, ());
        }
        if assorted.len() >= 10 {
            break;
        }
    }
    for entry in &pool {
        if assorted.len() >= 10 {
            break;
        }
        if !assorted.contains(entry) {
            assorted.push(*entry);
        }
    }
    for &(_, i, kind) in assorted.iter().take(10) {
        let safe = kind.replace('/', "_");
        chosen.push((format!("assorted_{safe}_{i}"), i));
    }

    let mut total = 0usize;
    for (name, i) in &chosen {
        let out = out_dir.join(format!("{name}.csi.bin"));
        std::fs::write(&out, &rends[*i].val).unwrap();
        total += rends[*i].val.len();
        println!("wrote {} ({} bytes)", out.display(), rends[*i].val.len());
    }
    let mut census: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rends {
        *census.entry(r.kind.as_str()).or_default() += 1;
    }
    println!("total: {} fixtures, {} bytes", chosen.len(), total);
    println!("kind census: {census:?}");
}
