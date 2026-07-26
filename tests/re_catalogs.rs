//! Round-trip regression over real system catalogs exercising the reverse-engineered codecs;
//! each must decompile -> compile -> decompile byte-identically. Gated on the fixtures being present.

use std::path::{Path, PathBuf};

fn catalogs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_catalogs")
}

fn tmp(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("scar-recat-{}-{tag}", std::process::id()));
    d
}

#[test]
fn re_catalogs_round_trip_byte_identical() {
    let dir = catalogs_dir();
    if !dir.is_dir() {
        eprintln!("no tests/re_catalogs, skipping");
        return;
    }
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        // Every regular file in the directory is a catalog (some names are truncated).
        let entry = entry.unwrap();
        let car = entry.path();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let tag = format!("{checked}");
        let a = tmp(&format!("{tag}a"));
        let b = tmp(&format!("{tag}b.car"));
        let c = tmp(&format!("{tag}c"));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&c);

        scar::decompile::decompile(&car, &a, false)
            .unwrap_or_else(|e| panic!("decompile {car:?}: {e}"));
        scar::compile::compile(&a, &b).unwrap_or_else(|e| panic!("compile {car:?}: {e}"));
        scar::decompile::decompile(&b, &c, false)
            .unwrap_or_else(|e| panic!("re-decompile {car:?}: {e}"));

        assert!(
            dirs_identical(&a, &c),
            "round-trip for {car:?} is not byte-identical"
        );
        // a-vs-c can't catch a dropped BOM variable (both are our own decompiles), so compare against the ORIGINAL.
        let orig_vars = bom_var_names(&car);
        let rebuilt_vars = bom_var_names(&b);
        assert_eq!(
            orig_vars, rebuilt_vars,
            "rebuilt BOM variables differ from original for {car:?}"
        );
        checked += 1;

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&c);
        let _ = std::fs::remove_file(&b);
    }
    eprintln!("re_catalogs round-trip verified {checked} catalogs");
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
    for rel in &fa {
        if std::fs::read(a.join(rel)).ok() != std::fs::read(b.join(rel)).ok() {
            return false;
        }
    }
    true
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
