//! Shared helpers for the CoreUI-oracle example harnesses. Not an example
//! itself — each example pulls this in with `#[path = "common/util.rs"]`.

#![allow(dead_code)] // each example uses a subset

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn workdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("scar-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("creating workdir");
    d
}

#[path = "cuidump.rs"]
pub mod cuidump_mod;

pub fn cuidump(car: &Path, out: &Path, filter: Option<&str>) {
    cuidump_mod::dump(car, out, filter);
}

/// Run `assetutil -I`, returning (exit code, stdout).
pub fn assetutil(car: &Path) -> (i32, String) {
    let out = Command::new("/usr/bin/assetutil")
        .arg("-I")
        .arg(car)
        .output()
        .expect("running assetutil");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Parse a cuidump `.rgbaref` (raw "RGBA"-magic premultiplied dump).
pub fn read_rgbaref(path: &Path) -> (u32, u32, Vec<u8>) {
    let d = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        d.len() >= 12 && &d[0..4] == b"RGBA",
        "{}: not an RGBA dump",
        path.display()
    );
    let w = u32::from_le_bytes(d[4..8].try_into().unwrap());
    let h = u32::from_le_bytes(d[8..12].try_into().unwrap());
    (w, h, d[12..].to_vec())
}

pub fn premultiply(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// Straight RGBA -> premultiplied, in place.
pub fn premultiply_buf(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        for c in 0..3 {
            px[c] = premultiply(px[c], a);
        }
    }
}

/// Count channels differing by more than `tol` (usize::MAX on length mismatch).
pub fn channels_over_tol(a: &[u8], b: &[u8], tol: i32) -> usize {
    if a.len() != b.len() {
        return usize::MAX;
    }
    a.iter()
        .zip(b)
        .filter(|(x, y)| (**x as i32 - **y as i32).abs() > tol)
        .count()
}

pub fn max_abs_diff(a: &[u8], b: &[u8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as i32 - *y as i32).abs())
        .max()
        .unwrap_or(0)
}

/// Compare baseline dumps to their edited counterparts; returns (differing images, total compared, peak delta).
pub fn compare_dump_dirs(
    base: &Path,
    edited: &Path,
    tol: i32,
) -> (Vec<(usize, String)>, usize, i32) {
    let mut worst = Vec::new();
    let mut total = 0;
    let mut peak = 0;
    let mut names: Vec<_> = std::fs::read_dir(base)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rgbaref"))
        .collect();
    names.sort();
    for n in names {
        let q = edited.join(&n);
        if !q.exists() {
            println!("  WARN missing in edited: {n}");
            continue;
        }
        let (_, _, a) = read_rgbaref(&base.join(&n));
        let (_, _, b) = read_rgbaref(&q);
        total += 1;
        peak = peak.max(max_abs_diff(&a, &b));
        let d = channels_over_tol(&a, &b, tol);
        if d > 0 {
            worst.push((d, n));
        }
    }
    worst.sort_by(|a, b| b.cmp(a));
    (worst, total, peak)
}
