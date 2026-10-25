//! JPEG header parser.
//!
//! Deliberately dependency-free and header-only: a scan of 271 GiB across
//! 2.1 million files may not decode a single pixel. We read the marker stream
//! up to the first SOS and take everything we need from it:
//!
//! * dimensions, progressive flag, component count and chroma subsampling,
//! * the embedded quantisation tables, from which we estimate the stored
//!   quality the same way ImageMagick does (compare against the IJG table,
//!   pick the scale factor with the smallest total deviation),
//! * the presence and size of the metadata segments, so we can tell "this file
//!   still carries 40 KB of camera junk" from "this file was already stripped",
//! * the `optimised` COM marker the old `optimise-images` pipeline wrote, which
//!   is recorded but never used to skip a file: in one production tree 99.4% of
//!   JPEGs carried it while 20% of those were still above q85.

use std::io;

/// IJG "Annex K" luminance quantisation table, in natural (row-major) order.
const ANNEX_K_LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Maps a zigzag position (as stored in the file) to a natural index.
const ZIGZAG_TO_NATURAL: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The IJG table expressed in the file's zigzag storage order.
fn annex_k_luma_zigzag() -> [u16; 64] {
    let mut out = [0u16; 64];
    for (zz, &natural) in ZIGZAG_TO_NATURAL.iter().enumerate() {
        out[zz] = ANNEX_K_LUMA[natural];
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Subsampling {
    /// 4:4:4 or grayscale - no chroma reduction.
    None,
    /// 4:2:2
    H2V1,
    /// 4:2:0
    H2V2,
    #[default]
    Unknown,
}

impl Subsampling {
    pub fn label(self) -> &'static str {
        match self {
            Subsampling::None => "4:4:4",
            Subsampling::H2V1 => "4:2:2",
            Subsampling::H2V2 => "4:2:0",
            Subsampling::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub progressive: bool,
    pub precision: u8,
    pub subsampling: Subsampling,
    /// Estimated stored quality (1-100).
    pub quality: u8,
    /// False when the quantisation tables are not an IJG-scaled table set
    /// (17-23% of the real corpus). The estimate is then a rough guide only.
    pub quality_confident: bool,
    /// EXIF orientation tag; 1 (or absent) means upright.
    pub orientation: u16,
    pub has_icc: bool,
    pub icc_bytes: u32,
    pub has_exif: bool,
    pub has_xmp: bool,
    pub has_iptc: bool,
    /// Sum of all APPn and COM payload bytes: what stripping metadata would free.
    pub metadata_bytes: u32,
    /// COM segment contents, in file order. The old pipeline wrote `optimised`;
    /// this tool writes `mediaopt:<version>:<quality>`.
    pub comments: Vec<String>,
    /// Bytes consumed by the header up to and including SOS.
    pub header_bytes: u32,
    /// True when the input slice ended before SOS (headers larger than our read).
    pub truncated: bool,
}

impl JpegInfo {
    /// The legacy `optimise-images` marker. Recorded, never used to skip a file:
    /// 99.4% of one production tree carries it and 20% of those are still q95+.
    pub fn has_legacy_marker(&self) -> bool {
        self.comments.iter().any(|c| c.trim() == "optimised")
    }

    /// Our own marker, written into every file this tool changed. It is the
    /// cheap recovery path when the manifest is lost or a tree is restored.
    pub fn has_tool_marker(&self) -> bool {
        self.comments
            .iter()
            .any(|c| c.trim_start().starts_with("mediaopt:"))
    }

    /// The first COM payload, whichever wrote it.
    pub fn comment(&self) -> Option<&str> {
        self.comments.first().map(|s| s.as_str())
    }

    /// True when this file is a 3-component, 4:4:4 JPEG, where switching to
    /// 4:2:0 would visibly smear coloured edges (graphics, screenshots).
    pub fn is_full_chroma(&self) -> bool {
        self.subsampling == Subsampling::None && self.components == 3
    }
}

const SOI: u8 = 0xD8;
const SOS: u8 = 0xDA;
const EOI: u8 = 0xD9;

fn sof_components(marker: u8) -> bool {
    matches!(
        marker,
        0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF
    )
}

pub fn parse(buf: &[u8]) -> io::Result<JpegInfo> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != SOI {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a JPEG (missing SOI)",
        ));
    }
    let mut info = JpegInfo {
        orientation: 1,
        quality_confident: false,
        ..Default::default()
    };
    let mut luma_table: Option<[u16; 64]> = None;
    let mut chroma_table_seen = false;
    let mut pos = 2usize;

    loop {
        // Every marker starts with 0xFF; fill bytes of 0xFF are allowed.
        while pos < buf.len() && buf[pos] != 0xFF {
            pos += 1;
        }
        if pos + 1 >= buf.len() {
            info.truncated = true;
            break;
        }
        let mut marker_pos = pos;
        while marker_pos < buf.len() && buf[marker_pos] == 0xFF {
            marker_pos += 1;
        }
        if marker_pos >= buf.len() {
            info.truncated = true;
            break;
        }
        let marker = buf[marker_pos];
        pos = marker_pos + 1;

        if marker == SOS || marker == EOI {
            if marker == SOS {
                info.header_bytes = (pos + 1) as u32;
            }
            break;
        }
        // Markers without a length field.
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == SOI {
            continue;
        }
        if pos + 2 > buf.len() {
            info.truncated = true;
            break;
        }
        let seg_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        if seg_len < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JPEG segment with impossible length",
            ));
        }
        let data_start = pos + 2;
        let data_end = pos + seg_len;
        if data_end > buf.len() {
            info.truncated = true;
            break;
        }
        let data = &buf[data_start..data_end];
        info.metadata_bytes = info.metadata_bytes.saturating_add(data.len() as u32);

        match marker {
            m if sof_components(m) => {
                info.progressive = m == 0xC2 || m == 0xC6 || m == 0xCA || m == 0xCE;
                if data.len() >= 6 {
                    info.precision = data[0];
                    info.height = u16::from_be_bytes([data[1], data[2]]) as u32;
                    info.width = u16::from_be_bytes([data[3], data[4]]) as u32;
                    info.components = data[5];
                    if data.len() >= 6 + info.components as usize * 3 && info.components >= 3 {
                        let h0 = data[7] >> 4;
                        let v0 = data[7] & 0x0F;
                        let h1 = data[6 + 4] >> 4;
                        let v1 = data[6 + 4] & 0x0F;
                        info.subsampling = match (h0 as u32 * v0 as u32, h1 as u32, v1 as u32) {
                            _ if h1 == 0 || v1 == 0 => Subsampling::Unknown,
                            (1, _, _) if h0 == 1 && v0 == 1 => Subsampling::None,
                            (_, 1, 1) if h0 == 2 && v0 == 2 => Subsampling::H2V2,
                            (_, 1, 1) if h0 == 2 && v0 == 1 => Subsampling::H2V1,
                            (_, 1, 2) if h0 == 1 && v0 == 2 => Subsampling::Unknown,
                            _ if h0 == h1 && v0 == v1 => Subsampling::None,
                            _ if h0 == 2 * h1 && v0 == 2 * v1 => Subsampling::H2V2,
                            _ if h0 == 2 * h1 && v0 == v1 => Subsampling::H2V1,
                            _ => Subsampling::Unknown,
                        };
                    } else if info.components == 1 {
                        info.subsampling = Subsampling::None;
                    }
                }
            }
            0xDB => {
                let mut p = 0usize;
                while p < data.len() {
                    let pq_tq = data[p];
                    let precision_bits = pq_tq >> 4;
                    let table_id = pq_tq & 0x0F;
                    p += 1;
                    let entry_size = if precision_bits == 1 { 2 } else { 1 };
                    if p + 64 * entry_size > data.len() {
                        break;
                    }
                    let mut table = [0u16; 64];
                    for (i, slot) in table.iter_mut().enumerate() {
                        *slot = if entry_size == 2 {
                            u16::from_be_bytes([data[p + i * 2], data[p + i * 2 + 1]])
                        } else {
                            data[p + i] as u16
                        };
                    }
                    p += 64 * entry_size;
                    match table_id {
                        0 if luma_table.is_none() => luma_table = Some(table),
                        1 => chroma_table_seen = true,
                        _ => {}
                    }
                }
                let _ = chroma_table_seen;
            }
            0xFE => {
                let text = String::from_utf8_lossy(data)
                    .trim_end_matches('\0')
                    .to_string();
                info.comments.push(text);
            }
            0xE1 => {
                if data.starts_with(b"Exif\0\0") {
                    info.has_exif = true;
                    if let Some(o) = exif_orientation(&data[6..]) {
                        info.orientation = o;
                    }
                } else if data.starts_with(b"http://ns.adobe.com/xap/1.0/") {
                    info.has_xmp = true;
                }
            }
            0xE2 => {
                if data.starts_with(b"ICC_PROFILE\0") {
                    info.has_icc = true;
                    info.icc_bytes = info.icc_bytes.saturating_add(data.len() as u32);
                }
            }
            0xED => {
                info.has_iptc = true;
            }
            _ => {}
        }
        pos = data_end;
    }

    if let Some(table) = luma_table {
        let (q, sad) = estimate_quality(&table);
        info.quality = q;
        // A perfect IJG match has a small total deviation; custom tables show a
        // much larger residual because no scale factor reproduces them.
        info.quality_confident = sad <= 700;
    } else if !info.truncated {
        info.quality = 0;
        info.quality_confident = false;
    }

    Ok(info)
}

/// Returns (quality, total absolute deviation) for the best matching scale.
fn estimate_quality(table: &[u16; 64]) -> (u8, u32) {
    let reference = annex_k_luma_zigzag();
    let mut best = (0u8, u32::MAX);
    for q in 1..=100u32 {
        let scale = if q < 50 {
            5000 / q
        } else {
            200u32.saturating_sub(2 * q)
        };
        let mut sad = 0u32;
        for i in 0..64 {
            let scaled = ((reference[i] as u32 * scale + 50) / 100).clamp(1, 255);
            sad += scaled.abs_diff(table[i] as u32);
        }
        if sad < best.1 {
            best = (q as u8, sad);
        }
    }
    best
}

/// Minimal EXIF orientation reader: walk IFD0 and look for tag 0x0112.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |off: usize| -> Option<u16> {
        if off + 2 > tiff.len() {
            return None;
        }
        Some(if little {
            u16::from_le_bytes([tiff[off], tiff[off + 1]])
        } else {
            u16::from_be_bytes([tiff[off], tiff[off + 1]])
        })
    };
    let u32_at = |off: usize| -> Option<u32> {
        if off + 4 > tiff.len() {
            return None;
        }
        Some(if little {
            u32::from_le_bytes([tiff[off], tiff[off + 1], tiff[off + 2], tiff[off + 3]])
        } else {
            u32::from_be_bytes([tiff[off], tiff[off + 1], tiff[off + 2], tiff[off + 3]])
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd0 = u32_at(4)? as usize;
    let count = u16_at(ifd0)? as usize;
    for i in 0..count.min(512) {
        let entry = ifd0 + 2 + i * 12;
        let tag = u16_at(entry)?;
        if tag == 0x0112 {
            // Orientation is SHORT, stored in the value field's first 2 bytes.
            let raw = u16_at(entry + 8)?;
            return Some(raw);
        }
    }
    None
}

/// Insert a COM segment carrying `text` into a JPEG byte stream.
///
/// The marker goes after the leading APPn segments (so JFIF/Exif/ICC keep their
/// conventional position) and before the first structural marker. Any existing
/// `mediaopt:` marker is dropped first, so re-running never stacks them up.
///
/// Returns `None` when the input is not a parseable JPEG, in which case the
/// caller simply stores the file as it is.
pub fn with_tool_marker(bytes: &[u8], text: &str) -> Option<Vec<u8>> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != SOI {
        return None;
    }
    // Collect the leading APPn/COM segments, dropping any previous marker of ours,
    // and find where the structural part of the file begins.
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut pos = 2usize;
    let mut insert_at = None;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF {
            insert_at = Some(pos);
            break;
        }
        let marker = bytes[pos + 1];
        if marker == SOS || marker == EOI || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            insert_at = Some(pos);
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > bytes.len() {
            insert_at = Some(pos);
            break;
        }
        let is_app = (0xE0..=0xEF).contains(&marker);
        if !is_app && marker != 0xFE {
            // A structural segment starts here: our marker goes just before it.
            insert_at = Some(pos);
            break;
        }
        let data = &bytes[pos + 4..pos + 2 + seg_len];
        if marker == 0xFE
            && String::from_utf8_lossy(data)
                .trim_start()
                .starts_with("mediaopt:")
        {
            // Ours, and stale: drop it. The loop always advances, so this cannot spin.
            pos += 2 + seg_len;
            continue;
        }
        segments.push((pos, pos + 2 + seg_len));
        pos += 2 + seg_len;
    }
    let insert_at = insert_at.or(Some(pos))?;

    let payload = text.as_bytes();
    let seg_len = payload.len() + 2;
    if seg_len > u16::MAX as usize {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + seg_len + 2);
    out.extend_from_slice(&bytes[0..2]); // SOI
    for (start, end) in segments {
        out.extend_from_slice(&bytes[start..end]);
    }
    out.push(0xFF);
    out.push(0xFE);
    out.extend_from_slice(&(seg_len as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&bytes[insert_at..]);
    Some(out)
}

/// The marker text this tool writes. Kept short: a COM segment costs 4 + len bytes.
///
/// `descriptor` is `q<quality>` for a lossy re-encode and `ll` for a lossless
/// transcode, so a future reader can tell what happened to the file.
pub fn tool_marker(version: &str, descriptor: &str) -> String {
    format!("mediaopt:{version}:{descriptor}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_k_table_is_self_consistent() {
        let zz = annex_k_luma_zigzag();
        assert_eq!(zz[0], 16); // DC term
        assert_eq!(zz[63], 99); // last AC term in zigzag order
    }

    #[test]
    fn quality_estimate_recovers_annex_k_at_q50() {
        // At q=50 the scale is 100, so the stored table *is* the IJG table.
        let reference = annex_k_luma_zigzag();
        let (q, sad) = estimate_quality(&reference);
        assert_eq!(q, 50, "annex K table should read back as q50");
        assert_eq!(sad, 0);
    }

    #[test]
    fn quality_estimate_recovers_q85() {
        let reference = annex_k_luma_zigzag();
        let scale = 200 - 2 * 85; // q=85
        let mut table = [0u16; 64];
        for i in 0..64 {
            table[i] = ((reference[i] as u32 * scale + 50) / 100).clamp(1, 255) as u16;
        }
        let (q, sad) = estimate_quality(&table);
        assert_eq!(q, 85);
        assert!(sad <= 2);
    }

    fn minimal_jpeg() -> Vec<u8> {
        // SOI, APP0/JFIF, COM("optimised"), DQT, SOF0, SOS, EOI - enough structure
        // for the marker logic without being a decodable image.
        let mut v: Vec<u8> = vec![0xFF, 0xD8];
        let app0 = b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0";
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, (app0.len() + 2) as u8]);
        v.extend_from_slice(app0);
        let com = b"optimised";
        v.extend_from_slice(&[0xFF, 0xFE, 0x00, (com.len() + 2) as u8]);
        v.extend_from_slice(com);
        v.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        v.extend_from_slice(&[0u8; 64]);
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn tool_marker_is_inserted_before_structural_markers() {
        let src = minimal_jpeg();
        let out = with_tool_marker(&src, "mediaopt:0.1:q85").unwrap();
        assert!(out.starts_with(&[0xFF, 0xD8]));
        // APP0 still immediately follows SOI.
        assert_eq!(&out[2..4], &[0xFF, 0xE0]);
        let info = parse(&out).unwrap();
        assert!(info.has_tool_marker());
        assert!(
            info.has_legacy_marker(),
            "existing comments must be preserved"
        );
    }

    #[test]
    fn tool_marker_does_not_stack() {
        let src = minimal_jpeg();
        let once = with_tool_marker(&src, "mediaopt:0.1:q85").unwrap();
        let twice = with_tool_marker(&once, "mediaopt:0.1:q85").unwrap();
        assert_eq!(once.len(), twice.len());
        assert_eq!(
            twice.iter().filter(|b| **b == 0xFE).count(),
            once.iter().filter(|b| **b == 0xFE).count()
        );
    }

    #[test]
    fn marker_text_is_short_and_parsable() {
        let m = tool_marker("0.1.0", "q85");
        assert_eq!(m, "mediaopt:0.1.0:q85");
        assert_eq!(tool_marker("0.1.0", "ll"), "mediaopt:0.1.0:ll");
        assert!(m.len() < 40);
    }

    #[test]
    fn rejects_non_jpeg() {
        assert!(parse(b"\x89PNG\r\n\x1a\n").is_err());
    }

    #[test]
    fn truncated_header_is_flagged_not_an_error() {
        // SOI, then a COM segment whose length runs past the buffer.
        let buf = [0xFF, 0xD8, 0xFF, 0xFE, 0x00, 0x40, b'o', b'k'];
        let info = parse(&buf).unwrap();
        assert!(info.truncated);
    }
}
