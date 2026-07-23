//! Shared constants and small value types for the CAR layer (little-endian).

/// Rendition key attribute identifiers (CoreUI theme attributes).
pub const ATTRIBUTE_NAMES: &[(u32, &str)] = &[
    (0, "look"),
    (1, "element"),
    (2, "part"),
    (3, "size"),
    (4, "direction"),
    (5, "placeholder"),
    (6, "value"),
    (7, "appearance"),
    (8, "dimension1"),
    (9, "dimension2"),
    (10, "state"),
    (11, "layer"),
    (12, "scale"),
    (13, "localization"),
    (14, "presentationState"),
    (15, "idiom"),
    (16, "subtype"),
    (17, "identifier"),
    (18, "previousValue"),
    (19, "previousState"),
    (20, "sizeClassHorizontal"),
    (21, "sizeClassVertical"),
    (22, "memoryClass"),
    (23, "graphicsClass"),
    (24, "displayGamut"),
    (25, "deploymentTarget"),
];

pub fn attribute_name(id: u32) -> String {
    ATTRIBUTE_NAMES
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("attr{id}"))
}

pub fn attribute_id(name: &str) -> Option<u32> {
    if let Some(rest) = name.strip_prefix("attr") {
        if let Ok(id) = rest.parse() {
            return Some(id);
        }
    }
    ATTRIBUTE_NAMES
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(i, _)| *i)
}

/// Magics as they appear on disk (byte strings, i.e. the LE-stored u32).
pub mod magic {
    pub const CAR_HEADER: &[u8; 4] = b"RATC"; // 'CTAR'
    pub const EXTENDED_METADATA: &[u8; 4] = b"META";
    pub const KEY_FORMAT: &[u8; 4] = b"tmfk"; // 'kfmt'
    pub const CSI: &[u8; 4] = b"ISTC"; // 'CTSI'
    pub const CELM: &[u8; 4] = b"MLEC"; // theme bitmap element
    pub const KCBC: &[u8; 4] = b"KCBC"; // compressed block chunk
    pub const RAWD: &[u8; 4] = b"DWAR"; // raw data
    pub const MSIS: &[u8; 4] = b"SISM"; // multisize image set
    pub const INLK: &[u8; 4] = b"KLNI"; // internal link
    pub const COLR: &[u8; 4] = b"RLOC"; // color
}

/// Pixel formats, stored as the raw LE u32 (ascii shown as on-disk bytes).
pub mod pixel_format {
    pub const NONE: u32 = 0;
    pub const ARGB: u32 = u32::from_le_bytes(*b"BGRA"); // 'ARGB', BGRA byte order
    pub const GA8: u32 = u32::from_le_bytes(*b" 8AG"); // 'GA8 ', gray+alpha
    pub const SVG: u32 = u32::from_le_bytes(*b" GVS"); // 'SVG '
    pub const DATA: u32 = u32::from_le_bytes(*b"ATAD"); // 'DATA'
    pub const JPEG: u32 = u32::from_le_bytes(*b"GEPJ"); // 'JPEG'
    pub const HEIF: u32 = u32::from_le_bytes(*b"FIEH"); // 'HEIF'
    pub const PDF: u32 = u32::from_le_bytes(*b" FDP"); // 'PDF '

    pub fn name(v: u32) -> String {
        match v {
            NONE => "none".into(),
            ARGB => "ARGB".into(),
            GA8 => "GA8".into(),
            SVG => "SVG".into(),
            DATA => "DATA".into(),
            JPEG => "JPEG".into(),
            HEIF => "HEIF".into(),
            PDF => "PDF".into(),
            other => format!("0x{other:08x}"),
        }
    }

    pub fn from_name(s: &str) -> Option<u32> {
        Some(match s {
            "none" => NONE,
            "ARGB" => ARGB,
            "GA8" => GA8,
            "SVG" => SVG,
            "DATA" => DATA,
            "JPEG" => JPEG,
            "HEIF" => HEIF,
            "PDF" => PDF,
            _ => {
                return s
                    .strip_prefix("0x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok());
            }
        })
    }
}

/// CELM compression types.
pub mod compression {
    pub const UNCOMPRESSED: u32 = 0;
    pub const RLE: u32 = 1;
    pub const ZLIB: u32 = 2;
    pub const LZVN: u32 = 3;
    pub const LZFSE: u32 = 4;
    pub const JPEG_LZFSE: u32 = 5;
    pub const BLURRED: u32 = 6;
    pub const ASTC: u32 = 7;
    pub const PALETTE_IMG: u32 = 8;
    pub const HEVC: u32 = 9;
    pub const DEEPMAP_LOSSLESS: u32 = 10;
    pub const DEEPMAP2: u32 = 11;

    pub fn name(v: u32) -> String {
        match v {
            UNCOMPRESSED => "uncompressed".into(),
            RLE => "rle".into(),
            ZLIB => "zlib".into(),
            LZVN => "lzvn".into(),
            LZFSE => "lzfse".into(),
            JPEG_LZFSE => "jpeg-lzfse".into(),
            BLURRED => "blurred".into(),
            ASTC => "astc".into(),
            PALETTE_IMG => "palette-img".into(),
            HEVC => "hevc".into(),
            DEEPMAP_LOSSLESS => "deepmap-lossless".into(),
            DEEPMAP2 => "deepmap2".into(),
            other => format!("{other}"),
        }
    }

    pub fn from_name(s: &str) -> Option<u32> {
        Some(match s {
            "uncompressed" => UNCOMPRESSED,
            "rle" => RLE,
            "zlib" => ZLIB,
            "lzvn" => LZVN,
            "lzfse" => LZFSE,
            "jpeg-lzfse" => JPEG_LZFSE,
            "blurred" => BLURRED,
            "astc" => ASTC,
            "palette-img" => PALETTE_IMG,
            "hevc" => HEVC,
            "deepmap-lossless" => DEEPMAP_LOSSLESS,
            "deepmap2" => DEEPMAP2,
            _ => return s.parse().ok(),
        })
    }
}

/// TLV tags in the CSI info list.
pub mod tlv {
    pub const SLICES: u32 = 0x3E9;
    pub const METRICS: u32 = 0x3EB;
    pub const COMPOSITION: u32 = 0x3EC;
    pub const UTI: u32 = 0x3ED;
    pub const BITMAP_INFO: u32 = 0x3EE;
    pub const BYTES_PER_ROW: u32 = 0x3EF;
    pub const REFERENCE: u32 = 0x3F0;
    pub const INTERNAL_LINK: u32 = 0x3F2;
}

/// Rendition layouts we branch on. Everything else is carried verbatim.
pub mod layout {
    pub const VECTOR: u16 = 9;
    pub const IMAGE: u16 = 12;
    pub const INTERNAL_LINK: u16 = 1003;
    pub const PACKED_IMAGE: u16 = 1004;
    pub const MULTISIZE_SET: u16 = 1010;
}

/// Round `width * bpp` up to the 32-byte multiple CoreUI uses for BGRA rows.
pub fn bytes_per_row(width: u32, bytes_per_pixel: u32) -> u32 {
    (width * bytes_per_pixel).div_ceil(32) * 32
}
