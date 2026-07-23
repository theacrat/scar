//! Ground-truth reference dumper CLI: decode every named image in a .car via
//! Apple's private CUICatalog and write raw "RGBA"-magic premultiplied dumps
//! (see examples/common/cuidump.rs for the implementation). macOS only.
//!
//! Note: the refs committed under tests/ are additionally LZFSE-wrapped
//! (see tests/APPLE_ASSETS_NOTICE.md); this tool emits raw dumps.
//!
//! Usage: cargo run --release --example cuidump -- <car> <outdir> [namefilter]

use std::path::Path;

#[path = "common/cuidump.rs"]
mod cuidump;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(car), Some(outdir)) = (args.first(), args.get(1)) else {
        eprintln!("usage: cargo run --release --example cuidump -- <car> <outdir> [namefilter]");
        std::process::exit(2);
    };
    let n = cuidump::dump(
        Path::new(car),
        Path::new(outdir),
        args.get(2).map(String::as_str),
    );
    println!("dumped {n} images");
}
