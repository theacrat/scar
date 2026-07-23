//! ARGG payload (layout 1021): a linear gradient — axis start/end points in
//! unit-square coordinates plus `(location, colorName)` stops, where each name
//! is the CSI `name` of a sibling COLR rendition CoreUI resolves at render time.
//!
//! On-disk layout (little-endian), 32-byte fixed header + variable stops:
//! ```text
//! 0   4  magic "ARGG"
//! 4   4  u32 stopCount
//! 8   4  u32 gradientType  (opaque enum, observed 0/1; NOT derived from stopCount)
//! 12  4  u32 reserved      (always 0 in samples)
//! 16  4  f32 start.x
//! 20  4  f32 start.y
//! 24  4  f32 end.x
//! 28  4  f32 end.y
//! then `stopCount` * {
//!   f32 location
//!   u32 nameLen      (strlen + 1 — includes the NUL terminator)
//!   nameLen bytes    name (UTF-8, NUL-terminated)
//! }
//! ```
//! Sibling magics "ARGA"/"ARGN" (other gradient variants) are not implemented;
//! `decode` returns Ok(None) for non-ARGG payloads so callers pass through.
//! CoreUI logs and ignores trailing bytes after the stops; `trailing` preserves
//! them so decode+encode stays byte-exact.

use anyhow::{Context, Result, bail};

/// One color stop: axis location and the referenced COLR rendition's name.
#[derive(Debug, Clone, PartialEq)]
pub struct GradientStop {
    pub location: f32,
    /// Raw on-disk name bytes, including the trailing NUL.
    pub name: Vec<u8>,
}

impl GradientStop {
    /// Lossy UTF-8 name without the trailing NUL.
    pub fn name_str(&self) -> String {
        let bytes = match self.name.split_last() {
            Some((0, rest)) => rest,
            _ => &self.name[..],
        };
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Structured ARGG (linear/axis gradient) contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Argg {
    /// Opaque enum (observed 0/1); preserved verbatim, NOT derived from stop count.
    pub gradient_type: u32,
    /// Always 0 in samples; preserved verbatim.
    pub reserved: u32,
    /// Axis start point (unit-square coordinates).
    pub start: (f32, f32),
    /// Axis end point (unit-square coordinates).
    pub end: (f32, f32),
    pub stops: Vec<GradientStop>,
    /// Bytes past the parsed stops, preserved verbatim.
    pub trailing: Vec<u8>,
}

const MAGIC: &[u8; 4] = b"ARGG";
const HEADER_LEN: usize = 32;

fn read_u32(data: &[u8], off: usize) -> Result<u32> {
    let b: [u8; 4] = data
        .get(off..off + 4)
        .context("truncated ARGG payload")?
        .try_into()
        .unwrap();
    Ok(u32::from_le_bytes(b))
}

fn read_f32(data: &[u8], off: usize) -> Result<f32> {
    let b: [u8; 4] = data
        .get(off..off + 4)
        .context("truncated ARGG payload")?
        .try_into()
        .unwrap();
    Ok(f32::from_le_bytes(b))
}

/// Parse an ARGG payload; Ok(None) when the magic isn't "ARGG"
/// (including the undecoded "ARGA"/"ARGN" variants).
pub fn decode(payload: &[u8]) -> Result<Option<Argg>> {
    if payload.len() < 4 || &payload[0..4] != MAGIC {
        return Ok(None);
    }
    if payload.len() < HEADER_LEN {
        bail!("ARGG payload too short: {} bytes", payload.len());
    }
    let count = read_u32(payload, 4)? as usize;
    let gradient_type = read_u32(payload, 8)?;
    let reserved = read_u32(payload, 12)?;
    let start = (read_f32(payload, 16)?, read_f32(payload, 20)?);
    let end = (read_f32(payload, 24)?, read_f32(payload, 28)?);

    let mut stops = Vec::with_capacity(count);
    let mut off = HEADER_LEN;
    for _ in 0..count {
        let location = read_f32(payload, off)?;
        let name_len = read_u32(payload, off + 4)? as usize;
        let name = payload
            .get(off + 8..off + 8 + name_len)
            .context("truncated ARGG color stop name")?
            .to_vec();
        stops.push(GradientStop { location, name });
        off += 8 + name_len;
    }
    let trailing = payload.get(off..).unwrap_or(&[]).to_vec();

    Ok(Some(Argg {
        gradient_type,
        reserved,
        start,
        end,
        stops,
        trailing,
    }))
}

/// Re-encode an ARGG payload from its structured form, byte-exact.
pub fn encode(argg: &Argg) -> Vec<u8> {
    let stops_len: usize = argg.stops.iter().map(|s| 8 + s.name.len()).sum();
    let mut out = Vec::with_capacity(HEADER_LEN + stops_len + argg.trailing.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(argg.stops.len() as u32).to_le_bytes());
    out.extend_from_slice(&argg.gradient_type.to_le_bytes());
    out.extend_from_slice(&argg.reserved.to_le_bytes());
    out.extend_from_slice(&argg.start.0.to_le_bytes());
    out.extend_from_slice(&argg.start.1.to_le_bytes());
    out.extend_from_slice(&argg.end.0.to_le_bytes());
    out.extend_from_slice(&argg.end.1.to_le_bytes());
    for s in &argg.stops {
        out.extend_from_slice(&s.location.to_le_bytes());
        out.extend_from_slice(&(s.name.len() as u32).to_le_bytes());
        out.extend_from_slice(&s.name);
    }
    out.extend_from_slice(&argg.trailing);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fixtures_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/re_fixtures")
    }

    fn argg_fixtures() -> Vec<(String, Vec<u8>)> {
        let dir = fixtures_dir();
        if !dir.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("argg_") && name.ends_with(".bin") {
                out.push((name, fs::read(entry.path()).unwrap()));
            }
        }
        out
    }

    #[test]
    fn decode_rejects_non_argg_magic() {
        assert!(decode(b"XXXX").unwrap().is_none());
        assert!(decode(b"").unwrap().is_none());
        assert!(decode(b"ARGA\0\0\0\0").unwrap().is_none());
    }

    #[test]
    fn fixtures_decode_and_round_trip_byte_exact() {
        let fixtures = argg_fixtures();
        if fixtures.is_empty() {
            eprintln!("no argg_*.bin fixtures found, skipping");
            return;
        }
        for (name, blob) in fixtures {
            let argg = decode(&blob)
                .unwrap_or_else(|e| panic!("{name}: decode failed: {e}"))
                .unwrap_or_else(|| panic!("{name}: expected Some(Argg)"));
            assert!(
                argg.trailing.is_empty(),
                "{name}: unexpected trailing bytes"
            );
            let out = encode(&argg);
            assert_eq!(out, blob, "{name}: byte-exact round trip failed");
        }
    }

    #[test]
    fn setup_two_stop_gradient_has_expected_axis_and_stops() {
        let Some((name, blob)) = argg_fixtures()
            .into_iter()
            .find(|(n, _)| n == "argg_setup_gradient1.bin")
        else {
            eprintln!("argg_setup_gradient1.bin not found, skipping");
            return;
        };
        let argg = decode(&blob)
            .unwrap()
            .unwrap_or_else(|| panic!("{name}: not recognized as ARGG"));
        assert_eq!(argg.start, (0.5, 0.0));
        assert_eq!(argg.end, (0.5, 1.0));
        assert_eq!(argg.gradient_type, 1);
        assert_eq!(argg.reserved, 0);
        assert_eq!(argg.stops.len(), 2);
        assert_eq!(argg.stops[0].location, 0.0);
        assert_eq!(argg.stops[0].name_str(), "AppIcon_Assets/Color-2");
        assert_eq!(argg.stops[1].location, 1.0);
        assert_eq!(argg.stops[1].name_str(), "AppIcon_Assets/Color-3");
    }

    #[test]
    fn setup_single_stop_gradient_has_expected_fields() {
        let Some((name, blob)) = argg_fixtures()
            .into_iter()
            .find(|(n, _)| n == "argg_setup_gradient3.bin")
        else {
            eprintln!("argg_setup_gradient3.bin not found, skipping");
            return;
        };
        let argg = decode(&blob)
            .unwrap()
            .unwrap_or_else(|| panic!("{name}: not recognized as ARGG"));
        assert_eq!(argg.gradient_type, 0);
        assert_eq!(argg.stops.len(), 1);
        assert_eq!(argg.stops[0].location, 0.0);
        assert_eq!(argg.stops[0].name_str(), "AppIcon_Assets/Color-6");
    }

    #[test]
    fn name_str_strips_trailing_nul() {
        let stop = GradientStop {
            location: 0.0,
            name: b"hello\0".to_vec(),
        };
        assert_eq!(stop.name_str(), "hello");
        let no_nul = GradientStop {
            location: 0.0,
            name: b"world".to_vec(),
        };
        assert_eq!(no_nul.name_str(), "world");
    }

    #[test]
    fn synthetic_round_trip_with_trailing_bytes() {
        let argg = Argg {
            gradient_type: 3,
            reserved: 0,
            start: (0.0, 0.5),
            end: (1.0, 0.5),
            stops: vec![
                GradientStop {
                    location: 0.0,
                    name: b"A\0".to_vec(),
                },
                GradientStop {
                    location: 0.25,
                    name: b"Some/Longer.Name-1\0".to_vec(),
                },
                GradientStop {
                    location: 1.0,
                    name: b"Z\0".to_vec(),
                },
            ],
            trailing: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let bytes = encode(&argg);
        let back = decode(&bytes).unwrap().unwrap();
        assert_eq!(back, argg);
    }
}
