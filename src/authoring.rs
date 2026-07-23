//! Catalog authoring: `scar pack` (folder of images -> decompiled dir) and
//! `scar clone-asset`; both produce output consumable by `compile`.
//!
//! Inputs: `.xcassets`-style `Foo.imageset/Contents.json` + PNGs, or plain
//! PNGs with `@2x`/`@3x` scale suffixes. Keys are minimal: element/identifier
//! get one unique id per asset name; duplicate (idiom, scale) images for an
//! asset are skipped with a warning.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::codec;
use crate::format;
use crate::manifest::{CarInfo, Content, ExtendedMetadata, Facet, Manifest, Rendition};

/// Options controlling catalog authoring.
pub struct PackOptions {
    /// Target platform string (e.g. "ios").
    pub platform: String,
    /// Deployment platform version (e.g. "15.0").
    pub platform_version: String,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            platform: "ios".into(),
            platform_version: "15.0".into(),
        }
    }
}

/// The 12 standard KEYFORMAT attribute names, in on-disk order (docs/FORMAT.md §4).
pub fn default_key_format() -> Vec<String> {
    [
        "appearance",
        "localization",
        "scale",
        "idiom",
        "subtype",
        "dimension2",
        "dimension1",
        "sizeClassHorizontal",
        "sizeClassVertical",
        "identifier",
        "element",
        "part",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// One discovered source image, prior to assigning element/identifier ids.
struct SourceImage {
    /// Imageset folder name, or filename minus scale suffix and extension.
    asset_name: String,
    /// 1, 2, or 3; the CSI header's scale_factor is this * 100.
    scale: u32,
    /// 0 universal, 1 phone, 2 pad.
    idiom: u16,
    path: PathBuf,
}

/// Keep [A-Za-z0-9._-], replace everything else with '_'.
fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "_".to_string() } else { s }
}

/// GA8 when every pixel is gray (R==G==B), else ARGB; both carry alpha, so this is purely a size choice.
fn infer_pixel_format(px: &codec::Pixels) -> &'static str {
    let grayscale = px
        .rgba
        .chunks_exact(4)
        .all(|p| p[0] == p[1] && p[1] == p[2]);
    if grayscale { "GA8" } else { "ARGB" }
}

fn idiom_from_str(s: Option<&str>) -> u16 {
    match s {
        Some("iphone") => 1,
        Some("ipad") => 2,
        _ => 0,
    }
}

fn scale_from_str(s: Option<&str>) -> u32 {
    s.and_then(|s| s.strip_suffix('x'))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
}

/// Parse a plain (non-imageset) PNG filename into (asset name, scale).
fn parse_plain_name(path: &Path) -> Result<(String, u32)> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("non-utf8 filename: {}", path.display()))?;
    for (suffix, scale) in [("@3x", 3), ("@2x", 2), ("@1x", 1)] {
        if let Some(base) = stem.strip_suffix(suffix) {
            return Ok((base.to_string(), scale));
        }
    }
    Ok((stem.to_string(), 1))
}

/// Parse an `Foo.imageset/Contents.json` and push each listed image.
fn scan_imageset(dir: &Path, asset_name: &str, out: &mut Vec<SourceImage>) -> Result<()> {
    let contents_path = dir.join("Contents.json");
    let data = fs::read_to_string(&contents_path)
        .with_context(|| format!("reading {}", contents_path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&data)
        .with_context(|| format!("parsing {}", contents_path.display()))?;
    let images = json
        .get("images")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("{}: missing \"images\" array", contents_path.display()))?;

    for img in images {
        let Some(filename) = img.get("filename").and_then(|v| v.as_str()) else {
            // Slots without "filename" are normal (unprovided scales); skip.
            continue;
        };
        let scale = scale_from_str(img.get("scale").and_then(|v| v.as_str()));
        let idiom = idiom_from_str(img.get("idiom").and_then(|v| v.as_str()));
        let path = dir.join(filename);
        if !path.exists() {
            eprintln!(
                "warning: {}: referenced file {filename:?} not found, skipping",
                contents_path.display()
            );
            continue;
        }
        out.push(SourceImage {
            asset_name: asset_name.to_string(),
            scale,
            idiom,
            path,
        });
    }
    Ok(())
}

/// Recursively walk `dir`, collecting `.imageset` bundles and loose PNGs.
fn scan_dir(dir: &Path, out: &mut Vec<SourceImage>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_dir() {
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if let Some(asset_name) = dir_name.strip_suffix(".imageset") {
                scan_imageset(&path, asset_name, out)?;
            } else {
                // Recurse; loose PNGs in un-special-cased containers (.appiconset etc.) are picked up as plain images.
                scan_dir(&path, out)?;
            }
        } else if file_type.is_file() {
            let is_png = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("png"));
            if is_png {
                let (asset_name, scale) = parse_plain_name(&path)?;
                out.push(SourceImage {
                    asset_name,
                    scale,
                    idiom: 0,
                    path,
                });
            }
        }
    }
    Ok(())
}

/// Deterministic 16-byte pseudo-uuid from the asset names, so repeated packs match.
fn derive_uuid(names: &[String]) -> String {
    use std::collections::hash_map::DefaultHasher;

    let mut h1 = DefaultHasher::new();
    "scar-pack-uuid-1".hash(&mut h1);
    for n in names {
        n.hash(&mut h1);
    }
    let a = h1.finish();

    let mut h2 = DefaultHasher::new();
    "scar-pack-uuid-2".hash(&mut h2);
    a.hash(&mut h2);
    for n in names.iter().rev() {
        n.hash(&mut h2);
    }
    let b = h2.finish();

    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&a.to_le_bytes());
    bytes[8..16].copy_from_slice(&b.to_le_bytes());
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Scan `input` for PNGs and write a decompiled-form directory ready for `compile`.
pub fn pack(input: &Path, out_dir: &Path, opts: &PackOptions) -> Result<()> {
    let mut images = Vec::new();
    if input.is_dir() {
        scan_dir(input, &mut images)?;
    } else if input.is_file() {
        let is_png = input
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"));
        if !is_png {
            bail!("{}: not a PNG file", input.display());
        }
        let (asset_name, scale) = parse_plain_name(input)?;
        images.push(SourceImage {
            asset_name,
            scale,
            idiom: 0,
            path: input.to_path_buf(),
        });
    } else {
        bail!("{}: no such file or directory", input.display());
    }

    if images.is_empty() {
        bail!("no PNG images found under {}", input.display());
    }

    // Deterministic ordering across runs.
    images.sort_by(|a, b| {
        (a.asset_name.as_str(), a.idiom, a.scale).cmp(&(b.asset_name.as_str(), b.idiom, b.scale))
    });

    // One 1-based element/identifier id per asset name; key attribute 0 means absent.
    let mut asset_names: Vec<String> = images.iter().map(|i| i.asset_name.clone()).collect();
    asset_names.sort();
    asset_names.dedup();
    if asset_names.len() > u16::MAX as usize {
        bail!(
            "too many distinct assets ({}) to encode as u16 element ids",
            asset_names.len()
        );
    }
    let element_of: BTreeMap<String, u16> = asset_names
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.clone(), (idx + 1) as u16))
        .collect();

    let renditions_dir = out_dir.join("renditions");
    fs::create_dir_all(&renditions_dir)
        .with_context(|| format!("creating {}", renditions_dir.display()))?;

    let mut seen_keys: HashSet<(u16, u16, u32)> = HashSet::new();
    let mut out_renditions: Vec<Rendition> = Vec::with_capacity(images.len());

    for (idx, img) in images.iter().enumerate() {
        let element = *element_of
            .get(&img.asset_name)
            .expect("asset_name was collected into element_of above");
        let dedup_key = (element, img.idiom, img.scale);
        if !seen_keys.insert(dedup_key) {
            eprintln!(
                "warning: asset {:?} already has an image at idiom={} scale={}x, skipping {}",
                img.asset_name,
                img.idiom,
                img.scale,
                img.path.display()
            );
            continue;
        }

        let px = codec::read_png(&img.path)
            .with_context(|| format!("reading {}", img.path.display()))?;
        let pixel_format = infer_pixel_format(&px);

        let mut key: BTreeMap<String, u16> = BTreeMap::new();
        key.insert("element".to_string(), element);
        key.insert("identifier".to_string(), element);
        if img.scale != 0 {
            key.insert("scale".to_string(), img.scale as u16);
        }
        if img.idiom != 0 {
            key.insert("idiom".to_string(), img.idiom);
        }

        let name = format!("{}.png", img.asset_name);
        let rel_file = format!("renditions/{idx:03}-{}", sanitize_name(&name));
        fs::copy(&img.path, out_dir.join(&rel_file))
            .with_context(|| format!("copying {} -> {rel_file}", img.path.display()))?;

        out_renditions.push(Rendition {
            key,
            name,
            layout: format::layout::IMAGE,
            flags: 0,
            pixel_format: pixel_format.to_string(),
            color_space_id: 1,
            width: px.width,
            height: px.height,
            scale: img.scale * 100,
            modified: 0,
            slices: None,
            metrics: None,
            composition: None,
            bitmap_info: None,
            extra_tlvs: BTreeMap::new(),
            content: Content::Image {
                file: rel_file,
                compression: "lzfse".to_string(),
                original: None,
                edit_hash: None,
            },
        });
    }

    // One facet per asset name, tying its element/identifier together.
    let facets: Vec<Facet> = asset_names
        .iter()
        .map(|name| {
            let element = *element_of.get(name).unwrap();
            let mut attributes = BTreeMap::new();
            attributes.insert("element".to_string(), element);
            attributes.insert("identifier".to_string(), element);
            Facet {
                name: name.clone(),
                hotspot: None,
                attributes,
            }
        })
        .collect();

    let mut appearances = BTreeMap::new();
    appearances.insert("UIAppearanceAny".to_string(), 0u16);

    let manifest = Manifest {
        car: CarInfo {
            coreui_version: 974,
            storage_version: 17,
            storage_timestamp: 0,
            main_version_string: "@(#)PROGRAM:CoreUI  PROJECT:CoreUI-974.1".to_string(),
            version_string: format!("scar {} via `scar pack`", env!("CARGO_PKG_VERSION")),
            uuid: derive_uuid(&asset_names),
            associated_checksum: 0,
            schema_version: 2,
            color_space_id: 1,
            key_semantics: 2,
            key_format: default_key_format(),
            metadata: Some(ExtendedMetadata {
                thinning_arguments: String::new(),
                deployment_platform_version: opts.platform_version.clone(),
                deployment_platform: opts.platform.clone(),
                authoring_tool: "scar".to_string(),
            }),
        },
        facets,
        appearances,
        localizations: BTreeMap::new(),
        renditions: out_renditions,
        bitmap_keys: BTreeMap::new(),
    };

    manifest.save(&out_dir.join(crate::manifest::MANIFEST_NAME))?;

    println!("Packed {} -> {}", input.display(), out_dir.display());
    println!("  assets:      {}", asset_names.len());
    println!("  renditions:  {}", manifest.renditions.len());
    println!("  facets:      {}", manifest.facets.len());

    Ok(())
}

/// Duplicate facet `from` and every rendition matching its identifier as `to`
/// under a fresh identifier, copying referenced files so the clone compiles
/// as-is. With `image`, install that PNG per `install_image`'s rules.
/// MSIS size tables reference siblings by dimension2 + shared identifier;
/// both sides are copied unchanged, so those references stay consistent.
pub fn clone_asset(dir: &Path, from: &str, to: &str, image: Option<&Path>) -> Result<()> {
    let manifest_path = dir.join(crate::manifest::MANIFEST_NAME);
    let mut manifest = Manifest::load(&manifest_path)
        .with_context(|| format!("loading manifest.json from {}", dir.display()))?;

    let src_facet = manifest
        .facets
        .iter()
        .find(|f| f.name == from)
        .ok_or_else(|| anyhow!("no facet named {from:?} in {}", dir.display()))?
        .clone();
    if manifest.facets.iter().any(|f| f.name == to) {
        bail!("a facet named {to:?} already exists");
    }
    let src_ident = *src_facet.attributes.get("identifier").ok_or_else(|| {
        anyhow!("facet {from:?} has no identifier attribute; cannot match its renditions")
    })?;

    // Fresh identifier unused by any facet or rendition key (0 = absent, so start at 1).
    let used: HashSet<u16> = manifest
        .facets
        .iter()
        .filter_map(|f| f.attributes.get("identifier"))
        .chain(
            manifest
                .renditions
                .iter()
                .filter_map(|r| r.key.get("identifier")),
        )
        .copied()
        .collect();
    let new_ident = (1..=u16::MAX)
        .find(|id| !used.contains(id))
        .ok_or_else(|| anyhow!("all u16 identifiers are in use"))?;

    let src_indices: Vec<usize> = manifest
        .renditions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.key.get("identifier") == Some(&src_ident))
        .map(|(i, _)| i)
        .collect();
    if src_indices.is_empty() {
        bail!("facet {from:?} (identifier {src_ident}) has no renditions");
    }

    let image_px = image
        .map(|p| codec::read_png(p).with_context(|| format!("reading {}", p.display())))
        .transpose()?;

    // Reject an --image that fits no bitmap rendition before writing anything (scar does not resample).
    if let Some(img) = &image_px {
        let installable = |r: &Rendition| {
            matches!(
                &r.content,
                Content::Image { .. }
                    | Content::RawPayload {
                        preview: Some(_),
                        edit_hash: Some(_),
                        ..
                    }
            )
        };
        let sizes: Vec<(u32, u32)> = {
            let mut s: Vec<(u32, u32)> = src_indices
                .iter()
                .map(|&i| &manifest.renditions[i])
                .filter(|r| installable(r))
                .map(|r| (r.width, r.height))
                .collect();
            s.sort();
            s.dedup();
            s
        };
        if !sizes.contains(&(img.width, img.height)) {
            let list = sizes
                .iter()
                .map(|(w, h)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "--image is {}x{} but no bitmap rendition of {from:?} has that exact size ({}); \
                 resample externally (scar does not resample)",
                img.width,
                img.height,
                if list.is_empty() {
                    "the asset has no installable bitmap renditions".to_string()
                } else {
                    format!("sizes: {list}")
                },
            );
        }
    }

    let mut clones: Vec<Rendition> = Vec::with_capacity(src_indices.len());
    let mut installed = 0usize;
    let mut skipped_links = 0usize;
    // Copied files are named by the clone's eventual rendition index.
    for (next_idx, &i) in (manifest.renditions.len()..).zip(src_indices.iter()) {
        let mut r = manifest.renditions[i].clone();
        r.key.insert("identifier".to_string(), new_ident);
        // MSIS stubs (and some bitmaps) carry the facet name as the CSI name.
        r.name = r.name.replace(from, to);

        match &mut r.content {
            Content::Image { file, original, .. } => {
                *file = copy_rendition_file(dir, file, next_idx, from, to)?;
                if let Some(orig) = original {
                    *orig = copy_rendition_file(dir, orig, next_idx, from, to)?;
                }
            }
            Content::Data { file, .. } => {
                *file = copy_rendition_file(dir, file, next_idx, from, to)?;
            }
            Content::RawPayload { file, preview, .. } => {
                *file = copy_rendition_file(dir, file, next_idx, from, to)?;
                if let Some(p) = preview {
                    *p = copy_rendition_file(dir, p, next_idx, from, to)?;
                }
            }
            Content::Link { preview, .. } => {
                if let Some(p) = preview {
                    *p = copy_rendition_file(dir, p, next_idx, from, to)?;
                }
            }
            Content::Multisize { .. } | Content::Color { .. } | Content::Gradient { .. } => {}
        }

        if let (Some(img), Some(image)) = (&image_px, image) {
            match install_image(dir, &r, image, img)? {
                InstallOutcome::Installed => installed += 1,
                InstallOutcome::SharedAtlas => skipped_links += 1,
                InstallOutcome::NotABitmap => {}
                InstallOutcome::SizeMismatch { need_w, need_h } => {
                    eprintln!(
                        "warning: {:?}: --image is {}x{} but this rendition needs {}x{}; \
                         kept the cloned art (edit the clone's PNG manually)",
                        r.name, img.width, img.height, need_w, need_h
                    );
                }
                InstallOutcome::NotEditable => {
                    eprintln!(
                        "warning: {:?}: rendition is not editable; kept the cloned payload",
                        r.name
                    );
                }
            }
        }

        clones.push(r);
    }
    if skipped_links > 0 {
        eprintln!(
            "warning: {skipped_links} link rendition(s) crop shared packed-atlas art also used by {from:?}; \
             their pixels were not replaced"
        );
    }

    let mut attributes = src_facet.attributes.clone();
    attributes.insert("identifier".to_string(), new_ident);
    manifest.facets.push(Facet {
        name: to.to_string(),
        hotspot: src_facet.hotspot,
        attributes,
    });
    let clone_count = clones.len();
    manifest.renditions.extend(clones);
    manifest.save(&manifest_path)?;

    println!("Cloned asset {from:?} -> {to:?} in {}", dir.display());
    println!("  identifier:  {src_ident} -> {new_ident}");
    println!("  renditions:  {clone_count}");
    if image.is_some() {
        println!("  image installed into {installed} rendition(s)");
    }

    Ok(())
}

pub(crate) enum InstallOutcome {
    Installed,
    SizeMismatch {
        need_w: u32,
        need_h: u32,
    },
    /// Link crops a packed atlas shared with the source; pasting through it would repaint the source too.
    SharedAtlas,
    /// A bitmap payload with no editable preview (unsupported codec).
    NotEditable,
    NotABitmap,
}

/// Overwrite the clone's PNG so `compile` re-encodes from it (Image directly;
/// RawPayload via its editable preview + hash mismatch).
pub(crate) fn install_image(
    dir: &Path,
    r: &Rendition,
    image: &Path,
    img: &codec::Pixels,
) -> Result<InstallOutcome> {
    let overwrite = |rel: &str| -> Result<()> {
        fs::copy(image, dir.join(rel))
            .with_context(|| format!("installing {} as {rel}", image.display()))?;
        Ok(())
    };
    match &r.content {
        Content::Image { file, .. } => {
            if (r.width, r.height) != (img.width, img.height) {
                return Ok(InstallOutcome::SizeMismatch {
                    need_w: r.width,
                    need_h: r.height,
                });
            }
            overwrite(file)?;
            Ok(InstallOutcome::Installed)
        }
        Content::RawPayload {
            preview: Some(p),
            edit_hash: Some(_),
            ..
        } => {
            if (r.width, r.height) != (img.width, img.height) {
                return Ok(InstallOutcome::SizeMismatch {
                    need_w: r.width,
                    need_h: r.height,
                });
            }
            overwrite(p)?;
            Ok(InstallOutcome::Installed)
        }
        Content::RawPayload { kind, .. } if kind.starts_with("celm-") => {
            Ok(InstallOutcome::NotEditable)
        }
        Content::Link { .. } => Ok(InstallOutcome::SharedAtlas),
        _ => Ok(InstallOutcome::NotABitmap),
    }
}

/// Copy a manifest-referenced file to a fresh `{idx:03}-` path, keeping its
/// subdir/extension and renaming `from` -> `to`; returns the new relative path.
fn copy_rendition_file(dir: &Path, rel: &str, idx: usize, from: &str, to: &str) -> Result<String> {
    let (subdir, fname) = rel.rsplit_once('/').unwrap_or(("renditions", rel));
    // Strip the "NNN-" rendition-index prefix decompile/pack put there.
    let base = match fname.split_once('-') {
        Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => fname,
    };
    let base = sanitize_name(&base.replace(from, to));
    let new_rel = format!("{subdir}/{idx:03}-{base}");
    fs::copy(dir.join(rel), dir.join(&new_rel))
        .with_context(|| format!("copying {rel} -> {new_rel}"))?;
    Ok(new_rel)
}
