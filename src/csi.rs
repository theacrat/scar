//! CSI rendition blob: 184-byte header + TLV info list + payload.
//! See docs/FORMAT.md §5. All fields little-endian.

use anyhow::{Context, Result, bail};

use crate::format::magic;

pub const HEADER_LEN: usize = 184;
const NAME_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct CsiHeader {
    pub version: u32, // 1
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: u32,
    pub pixel_format: u32,
    pub color_space_id: u32,
    pub mod_time: u32,
    pub layout: u16,
    /// Raw name bytes (zero padding stripped) so non-UTF8 names round-trip byte-perfect.
    pub name: Vec<u8>,
    pub unknown_a: u32, // 1 in the wild
    pub unknown_b: u32, // 0 in the wild
}

impl CsiHeader {
    /// Lossy UTF-8 view of the name, for display purposes.
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }

    pub fn set_name(&mut self, s: &str) {
        self.name = s.as_bytes().to_vec();
    }
}

#[derive(Debug, Clone)]
pub struct Tlv {
    pub tag: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Csi {
    pub header: CsiHeader,
    pub tlvs: Vec<Tlv>,
    pub payload: Vec<u8>,
}

fn read_u32(data: &[u8], off: usize) -> Result<u32> {
    let b: [u8; 4] = data
        .get(off..off + 4)
        .context("truncated CSI blob")?
        .try_into()
        .unwrap();
    Ok(u32::from_le_bytes(b))
}

fn read_u16(data: &[u8], off: usize) -> Result<u16> {
    let b: [u8; 2] = data
        .get(off..off + 2)
        .context("truncated CSI blob")?
        .try_into()
        .unwrap();
    Ok(u16::from_le_bytes(b))
}

impl Csi {
    pub fn parse(data: &[u8]) -> Result<Csi> {
        if data.len() < HEADER_LEN {
            bail!("CSI blob too short: {} bytes", data.len());
        }
        let magic_bytes = &data[0..4];
        if magic_bytes != magic::CSI {
            bail!(
                "bad CSI magic: {:?} (expected {:?})",
                magic_bytes,
                magic::CSI
            );
        }
        let version = read_u32(data, 4)?;
        let flags = read_u32(data, 8)?;
        let width = read_u32(data, 12)?;
        let height = read_u32(data, 16)?;
        let scale_factor = read_u32(data, 20)?;
        let pixel_format = read_u32(data, 24)?;
        let color_space_id = read_u32(data, 28)?;
        let mod_time = read_u32(data, 32)?;
        let layout = read_u16(data, 36)?;
        // offset 38: 2 zero bytes, not preserved separately (always 0 in the wild).
        let name_bytes = &data[40..40 + NAME_LEN];
        let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        let name = name_bytes[..name_end].to_vec();
        let info_list_len = read_u32(data, 168)? as usize;
        let unknown_a = read_u32(data, 172)?;
        let unknown_b = read_u32(data, 176)?;
        let payload_len = read_u32(data, 180)? as usize;

        let header = CsiHeader {
            version,
            flags,
            width,
            height,
            scale_factor,
            pixel_format,
            color_space_id,
            mod_time,
            layout,
            name,
            unknown_a,
            unknown_b,
        };

        let tlv_start = HEADER_LEN;
        let tlv_end = tlv_start
            .checked_add(info_list_len)
            .context("info list length overflow")?;
        let tlv_bytes = data
            .get(tlv_start..tlv_end)
            .context("CSI blob truncated: TLV info list")?;

        let mut tlvs = Vec::new();
        let mut p = 0usize;
        while p < tlv_bytes.len() {
            if p + 8 > tlv_bytes.len() {
                bail!("truncated TLV entry in CSI info list");
            }
            let tag = u32::from_le_bytes(tlv_bytes[p..p + 4].try_into().unwrap());
            let len = u32::from_le_bytes(tlv_bytes[p + 4..p + 8].try_into().unwrap()) as usize;
            p += 8;
            let end = p.checked_add(len).context("TLV length overflow")?;
            let tlv_data = tlv_bytes
                .get(p..end)
                .context("truncated TLV entry data")?
                .to_vec();
            tlvs.push(Tlv {
                tag,
                data: tlv_data,
            });
            p = end;
        }

        let payload_start = tlv_end;
        let payload_end = payload_start
            .checked_add(payload_len)
            .context("payload length overflow")?;
        let payload = data
            .get(payload_start..payload_end)
            .context("CSI blob truncated: payload")?
            .to_vec();

        Ok(Csi {
            header,
            tlvs,
            payload,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut tlv_bytes = Vec::new();
        for t in &self.tlvs {
            tlv_bytes.extend_from_slice(&t.tag.to_le_bytes());
            tlv_bytes.extend_from_slice(&(t.data.len() as u32).to_le_bytes());
            tlv_bytes.extend_from_slice(&t.data);
        }

        let mut out = Vec::with_capacity(HEADER_LEN + tlv_bytes.len() + self.payload.len());
        out.extend_from_slice(magic::CSI);
        out.extend_from_slice(&self.header.version.to_le_bytes());
        out.extend_from_slice(&self.header.flags.to_le_bytes());
        out.extend_from_slice(&self.header.width.to_le_bytes());
        out.extend_from_slice(&self.header.height.to_le_bytes());
        out.extend_from_slice(&self.header.scale_factor.to_le_bytes());
        out.extend_from_slice(&self.header.pixel_format.to_le_bytes());
        out.extend_from_slice(&self.header.color_space_id.to_le_bytes());
        out.extend_from_slice(&self.header.mod_time.to_le_bytes());
        out.extend_from_slice(&self.header.layout.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // zero field at offset 38

        let mut name_field = [0u8; NAME_LEN];
        let n = self.header.name.len().min(NAME_LEN);
        name_field[..n].copy_from_slice(&self.header.name[..n]);
        out.extend_from_slice(&name_field);

        out.extend_from_slice(&(tlv_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.header.unknown_a.to_le_bytes());
        out.extend_from_slice(&self.header.unknown_b.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());

        debug_assert_eq!(out.len(), HEADER_LEN);

        out.extend_from_slice(&tlv_bytes);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn tlv(&self, tag: u32) -> Option<&[u8]> {
        self.tlvs
            .iter()
            .find(|t| t.tag == tag)
            .map(|t| t.data.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn each_fixture() -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for dir in ["tests/fixtures"] {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("bin") {
                    let data = fs::read(&path).unwrap();
                    out.push((
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        data,
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn header_len_is_184() {
        assert_eq!(HEADER_LEN, 184);
    }

    #[test]
    fn fixtures_round_trip_byte_perfect() {
        let fixtures = each_fixture();
        if fixtures.is_empty() {
            eprintln!("no fixtures found, skipping (run `cargo run --example extract_fixtures`)");
            return;
        }
        for (name, blob) in fixtures {
            let csi = Csi::parse(&blob).unwrap_or_else(|e| panic!("{name}: parse failed: {e}"));
            let out = csi.to_bytes();
            assert_eq!(
                out.len(),
                blob.len(),
                "{name}: length mismatch after re-serialize"
            );
            assert_eq!(out, blob, "{name}: byte mismatch after re-serialize");

            let info_list_len = u32::from_le_bytes(blob[168..172].try_into().unwrap()) as usize;
            let payload_len = u32::from_le_bytes(blob[180..184].try_into().unwrap()) as usize;
            assert_eq!(
                payload_len,
                csi.payload.len(),
                "{name}: payloadLength field mismatch"
            );
            assert_eq!(
                HEADER_LEN + info_list_len + payload_len,
                blob.len(),
                "{name}: header+tlv+payload doesn't cover whole blob"
            );
        }
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut data = vec![0u8; HEADER_LEN];
        data[0..4].copy_from_slice(b"XXXX");
        assert!(Csi::parse(&data).is_err());
    }

    #[test]
    fn name_round_trips_through_string_and_bytes() {
        let mut header = CsiHeader {
            version: 1,
            flags: 0,
            width: 1,
            height: 1,
            scale_factor: 100,
            pixel_format: 0,
            color_space_id: 0,
            mod_time: 0,
            layout: 12,
            name: Vec::new(),
            unknown_a: 1,
            unknown_b: 0,
        };
        header.set_name("hello.png");
        assert_eq!(header.name_str(), "hello.png");
        let csi = Csi {
            header,
            tlvs: Vec::new(),
            payload: Vec::new(),
        };
        let bytes = csi.to_bytes();
        let reparsed = Csi::parse(&bytes).unwrap();
        assert_eq!(reparsed.header.name_str(), "hello.png");
        assert_eq!(bytes, reparsed.to_bytes());
    }
}
