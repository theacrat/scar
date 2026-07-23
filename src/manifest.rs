//! The `manifest.json` schema shared by `decompile` (writer) and `compile`
//! (reader). Paths inside the manifest are relative to the manifest file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub car: CarInfo,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<Facet>,
    /// Appearance name -> id (APPEARANCEKEYS tree).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub appearances: BTreeMap<String, u16>,
    /// Localization name -> id (LOCALIZATIONKEYS tree, e.g. "en" -> 6677).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub localizations: BTreeMap<String, u16>,
    pub renditions: Vec<Rendition>,
    /// Opaque BITMAPKEYS entries (inline u32 key -> base64), written back verbatim.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bitmap_keys: BTreeMap<u32, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarInfo {
    pub coreui_version: u32,
    pub storage_version: u32,
    #[serde(default)]
    pub storage_timestamp: u32,
    pub main_version_string: String,
    pub version_string: String,
    /// 16 bytes, hex.
    pub uuid: String,
    #[serde(default)]
    pub associated_checksum: u32,
    pub schema_version: u32,
    pub color_space_id: u32,
    pub key_semantics: u32,
    /// Attribute names in on-disk key order (KEYFORMAT).
    pub key_format: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ExtendedMetadata>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtendedMetadata {
    #[serde(default)]
    pub thinning_arguments: String,
    #[serde(default)]
    pub deployment_platform_version: String,
    #[serde(default)]
    pub deployment_platform: String,
    #[serde(default)]
    pub authoring_tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Facet {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotspot: Option<(u16, u16)>,
    /// Attribute name -> value pairs (element/part/identifier...).
    pub attributes: BTreeMap<String, u16>,
}

/// One rendition (an entry in the RENDITIONS tree).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rendition {
    /// Attribute name -> value; zero-valued attributes are omitted.
    pub key: BTreeMap<String, u16>,
    pub name: String,
    pub layout: u16,
    #[serde(default)]
    pub flags: u32,
    pub pixel_format: String,
    #[serde(default)]
    pub color_space_id: u32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub scale: u32,
    #[serde(default)]
    pub modified: u32,
    /// Slice rects (TLV 0x3E9), round-tripped verbatim; y origin is bottom-up (docs/FORMAT.md §6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slices: Option<Vec<[u32; 4]>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<Composition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitmap_info: Option<u32>,
    /// Unknown TLVs, tag (hex string) -> base64 data, preserved verbatim.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_tlvs: BTreeMap<String, String>,
    pub content: Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub edge_insets: [u32; 4], // top, left, bottom, right
    pub image_size: (u32, u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Composition {
    pub blend_mode: u32,
    pub opacity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Content {
    /// Decoded bitmap stored as a standard (non-premultiplied) PNG.
    /// `compression` is the CELM compression to use when compiling.
    Image {
        file: String,
        compression: String,
        /// Original CELM payload, kept when PNG re-encoding would be lossy (un/re-premultiply);
        /// written back verbatim unless the PNG changed (hash != `edit_hash`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit_hash: Option<String>,
    },
    /// Raw data rendition (SVG, PDF, arbitrary data). `lzfse` says whether the
    /// on-disk RAWD payload wraps the file in an LZFSE stream.
    Data { file: String, lzfse: bool },
    /// A crop into a packed atlas rendition.
    Link {
        /// Attribute name -> value of the target rendition key.
        target: BTreeMap<String, u16>,
        rect: [u32; 4],
        content_layout: u16,
        /// Crop render of the linked region. Editable when `edit_hash` is set: a changed
        /// preview is pasted back into the atlas at `rect`; the link record itself is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit_hash: Option<String>,
    },
    /// Multisize image set stub (layout 1010).
    Multisize { sizes: Vec<MultisizeEntry> },
    /// Color rendition. `system_color` is an optional referenced system-color
    /// name (e.g. "linkColor"); `extra` preserves any unmodeled trailing bytes.
    Color {
        color_space: u32,
        components: Vec<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_color: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        extra: String,
    },
    /// Named linear gradient (ARGG, layout 1021): axis points plus color stops
    /// that reference sibling `Color` renditions by name.
    Gradient {
        gradient_type: u32,
        #[serde(default)]
        reserved: u32,
        start: [f32; 2],
        end: [f32; 2],
        stops: Vec<GradientStopManifest>,
    },
    /// Verbatim CSI payload bytes for anything we cannot decode (deepmap2,
    /// rle, astc, ...). Guarantees lossless round-trip.
    RawPayload {
        file: String,
        /// Human hint only (e.g. "celm-deepmap2").
        kind: String,
        /// Optional preview PNG we managed to render anyway.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        /// When set, `preview` is editable: compile re-encodes from it if its hash
        /// changed, otherwise writes `file` verbatim for a byte-exact round-trip.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit_hash: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultisizeEntry {
    pub width: u32,
    pub height: u32,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradientStopManifest {
    pub location: f32,
    /// Name of the sibling `Color` rendition this stop resolves to.
    pub color_name: String,
}

impl Manifest {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
