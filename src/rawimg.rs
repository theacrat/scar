//! Container sniffing for renditions whose payload is an embedded image/data
//! stream (JPEG, HEIF, PDF, or opaque DATA) rather than a CoreUI bitmap.

use crate::codec::{rawd_decode, rawd_encode};
use crate::format::magic;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Jpeg,
    Heif,
    Png,
    Pdf,
    Data,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Jpeg => "jpg",
            Container::Heif => "heic",
            Container::Png => "png",
            Container::Pdf => "pdf",
            Container::Data => "bin",
        }
    }
}

/// Sniff the container kind of raw bytes by magic.
pub fn sniff(data: &[u8]) -> Option<Container> {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return Some(Container::Jpeg);
    }
    if data.len() >= 4 && data[0..4] == [0x89, 0x50, 0x4E, 0x47] {
        return Some(Container::Png);
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let brand = &data[8..12];
        const HEIF_BRANDS: &[&[u8; 4]] = &[b"heic", b"heix", b"hevc", b"mif1", b"msf1"];
        if HEIF_BRANDS.iter().any(|b| brand == *b as &[u8]) {
            return Some(Container::Heif);
        }
    }
    if data.len() >= 5 && &data[0..5] == b"%PDF-" {
        return Some(Container::Pdf);
    }
    None
}

/// Extension for the extracted file; "bin" when no magic matches.
pub fn detect_ext(data: &[u8]) -> &'static str {
    sniff(data).unwrap_or(Container::Data).ext()
}

pub fn is_image_container(c: Container) -> bool {
    matches!(c, Container::Jpeg | Container::Heif | Container::Png)
}

/// Unwrap a RAWD envelope if present (inflating LZFSE) and return the inner
/// bytes plus the file extension to write them with.
pub fn payload_to_file_bytes(payload: &[u8]) -> (Vec<u8>, &'static str) {
    let inner = if payload.len() >= 4 && &payload[0..4] == magic::RAWD {
        match rawd_decode(payload) {
            Ok((bytes, _wrapped)) => bytes,
            // Malformed RAWD wrapper: treat the whole payload as the stream.
            Err(_) => payload.to_vec(),
        }
    } else {
        payload.to_vec()
    };
    let ext = detect_ext(&inner);
    (inner, ext)
}

/// Inverse of [`payload_to_file_bytes`].
pub fn file_bytes_to_payload(data: &[u8], wrap_rawd: bool, lzfse: bool) -> Vec<u8> {
    if wrap_rawd {
        rawd_encode(data, lzfse)
    } else {
        data.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_jpeg() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
        v.extend_from_slice(b"\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        v.extend_from_slice(&[0xAB; 64]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn synthetic_heic() -> Vec<u8> {
        let mut v = Vec::new();
        // box size (arbitrary, not validated by sniff), then "ftyp", then brand.
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x18]);
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(b"heic");
        v.extend_from_slice(&[0, 0, 0, 0]); // minor version
        v.extend_from_slice(b"heic"); // compatible brand
        v
    }

    #[test]
    fn sniff_detects_jpeg() {
        let data = synthetic_jpeg();
        assert_eq!(sniff(&data), Some(Container::Jpeg));
        assert_eq!(detect_ext(&data), "jpg");
    }

    #[test]
    fn sniff_detects_png() {
        let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0; 16]);
        assert_eq!(sniff(&data), Some(Container::Png));
        assert_eq!(detect_ext(&data), "png");
    }

    #[test]
    fn sniff_detects_pdf() {
        let data = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n1 0 obj\n<< >>\nendobj".to_vec();
        assert_eq!(sniff(&data), Some(Container::Pdf));
        assert_eq!(detect_ext(&data), "pdf");
    }

    #[test]
    fn sniff_detects_synthetic_heic() {
        let data = synthetic_heic();
        assert_eq!(sniff(&data), Some(Container::Heif));
        assert_eq!(detect_ext(&data), "heic");
    }

    #[test]
    fn sniff_returns_none_for_unrecognized() {
        let data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        assert_eq!(sniff(&data), None);
        assert_eq!(detect_ext(&data), "bin");
    }

    #[test]
    fn is_image_container_classification() {
        assert!(is_image_container(Container::Jpeg));
        assert!(is_image_container(Container::Heif));
        assert!(is_image_container(Container::Png));
        assert!(!is_image_container(Container::Pdf));
        assert!(!is_image_container(Container::Data));
    }

    #[test]
    fn jpeg_round_trips_bare_payload() {
        let jpeg = synthetic_jpeg();
        let payload = file_bytes_to_payload(&jpeg, false, false);
        assert_eq!(payload, jpeg);
        let (recovered, ext) = payload_to_file_bytes(&payload);
        assert_eq!(recovered, jpeg);
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn jpeg_round_trips_through_rawd_uncompressed() {
        let jpeg = synthetic_jpeg();
        let payload = file_bytes_to_payload(&jpeg, true, false);
        assert_eq!(&payload[0..4], magic::RAWD);
        let (recovered, ext) = payload_to_file_bytes(&payload);
        assert_eq!(recovered, jpeg);
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn jpeg_round_trips_through_rawd_lzfse() {
        let jpeg = synthetic_jpeg();
        let payload = file_bytes_to_payload(&jpeg, true, true);
        assert_eq!(&payload[0..4], magic::RAWD);
        let (recovered, ext) = payload_to_file_bytes(&payload);
        assert_eq!(recovered, jpeg);
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn pdf_and_heic_round_trip_through_rawd() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF".to_vec();
        let payload = file_bytes_to_payload(&pdf, true, true);
        let (recovered, ext) = payload_to_file_bytes(&payload);
        assert_eq!(recovered, pdf);
        assert_eq!(ext, "pdf");

        let heic = synthetic_heic();
        let payload = file_bytes_to_payload(&heic, true, false);
        let (recovered, ext) = payload_to_file_bytes(&payload);
        assert_eq!(recovered, heic);
        assert_eq!(ext, "heic");
    }
}
