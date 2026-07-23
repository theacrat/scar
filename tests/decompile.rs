//! End-to-end decompile test against the real sample Assets.car. Only runs
//! when the sample file is present on disk (it isn't checked into the repo).
//! See docs/FORMAT.md §9 for the expected census.

use std::path::Path;

use scar::codec;
use scar::manifest::{Content, Manifest};

#[test]
fn decompiles_sample_car_with_expected_content_type_counts() {
    let car = Path::new("/Users/thea/Downloads/Assets.car");
    if !car.exists() {
        eprintln!("sample Assets.car not present, skipping whole-file decompile test");
        return;
    }

    let out = std::env::temp_dir().join(format!("scar-decompile-test-{}", std::process::id()));
    // Best-effort cleanup of any stale leftovers from a previous crashed run.
    let _ = std::fs::remove_dir_all(&out);

    let result = scar::decompile::decompile(car, &out, false);
    if let Err(e) = &result {
        let _ = std::fs::remove_dir_all(&out);
        panic!("decompile failed: {e}");
    }

    let manifest = Manifest::load(&out.join(scar::manifest::MANIFEST_NAME)).unwrap_or_else(|e| {
        let _ = std::fs::remove_dir_all(&out);
        panic!("loading manifest.json failed: {e}");
    });

    // Wrap the assertions so we can still clean up the tempdir on failure.
    // Catalog-agnostic: the sample file (Apple-copyrighted, kept out of the
    // repo) may be any real Assets.car, so assert invariants that hold for any
    // catalog rather than one specific census.
    let check = std::panic::catch_unwind(|| {
        assert!(
            !manifest.renditions.is_empty(),
            "catalog should have renditions"
        );
        assert!(
            !manifest.car.key_format.is_empty(),
            "catalog should have a key format"
        );

        let mut images = 0usize;
        let mut data_svg = 0usize;
        let mut links = 0usize;
        let mut link_previews = 0usize;
        let mut multisize = 0usize;
        let mut raw_deepmap2 = 0usize;
        let mut raw_rle = 0usize;
        let mut dmp2_previews = 0usize;

        for r in &manifest.renditions {
            match &r.content {
                Content::Image { .. } => images += 1,
                Content::Data { file, .. } => {
                    if file.ends_with(".svg") {
                        data_svg += 1;
                    }
                }
                Content::Link { preview, .. } => {
                    links += 1;
                    if preview.is_some() {
                        link_previews += 1;
                    }
                }
                Content::Multisize { .. } => multisize += 1,
                Content::RawPayload { kind, preview, .. } => match kind.as_str() {
                    "celm-deepmap2" => {
                        raw_deepmap2 += 1;
                        if preview.is_some() {
                            dmp2_previews += 1;
                        }
                    }
                    "celm-rle" => raw_rle += 1,
                    other => panic!("unexpected raw-payload kind: {other}"),
                },
                Content::Color { .. } => {}
                Content::Gradient { .. } => {}
            }
        }

        let _ = (data_svg, raw_rle, multisize);
        eprintln!(
            "census: images={images} deepmap2={raw_deepmap2} links={links} \
             deepmap2-previews={dmp2_previews}/{raw_deepmap2} link-previews={link_previews}/{links}"
        );

        let png_count = std::fs::read_dir(out.join("renditions")).unwrap().count();
        assert_eq!(
            png_count, images,
            "PNG file count on disk should match Image content entries"
        );

        // Spot-check: at least one written PNG opens and matches manifest dims.
        let mut checked = 0;
        for r in &manifest.renditions {
            if let Content::Image { file, .. } = &r.content {
                let px = codec::read_png(&out.join(file))
                    .unwrap_or_else(|e| panic!("reading {file}: {e}"));
                assert_eq!(px.width, r.width, "{file}: width mismatch");
                assert_eq!(px.height, r.height, "{file}: height mismatch");
                checked += 1;
                if checked >= 5 {
                    break;
                }
            }
        }
        assert!(
            checked > 0,
            "expected at least one Image rendition to spot-check"
        );
    });

    let _ = std::fs::remove_dir_all(&out);
    if let Err(e) = check {
        std::panic::resume_unwind(e);
    }
}
