//! PNG and WebP header parsers (header-only, like the JPEG one).

use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColour {
    Gray,
    Rgb,
    Palette,
    GrayAlpha,
    Rgba,
}

impl PngColour {
    pub fn label(self) -> &'static str {
        match self {
            PngColour::Gray => "gray",
            PngColour::Rgb => "rgb",
            PngColour::Palette => "palette",
            PngColour::GrayAlpha => "gray+alpha",
            PngColour::Rgba => "rgba",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PngInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub colour: Option<PngColour>,
    pub interlaced: bool,
    /// tRNS chunk present, or an alpha colour type.
    pub has_alpha: bool,
    pub has_palette: bool,
    pub has_icc: bool,
    pub has_exif: bool,
    pub metadata_bytes: u32,
    pub idat_bytes: u64,
}

impl PngInfo {
    /// Opaque, non-palette images are candidates for palette quantisation.
    /// A palette image is already quantified, so leave it to the lossless path.
    pub fn palette_candidate(&self) -> bool {
        !self.has_alpha && !self.has_palette && self.colour != Some(PngColour::Palette)
    }
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub fn parse(buf: &[u8]) -> io::Result<PngInfo> {
    if buf.len() < 8 || buf[0..8] != PNG_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a PNG (bad signature)",
        ));
    }
    let mut info = PngInfo::default();
    let mut pos = 8usize;
    let mut seen_ihdr = false;
    while pos + 8 <= buf.len() {
        let len = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        let kind = &buf[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = match data_start.checked_add(len) {
            Some(e) => e,
            None => break,
        };
        if data_end + 4 > buf.len() {
            break; // chunk runs past our read window; keep what we have
        }
        let data = &buf[data_start..data_end];
        match kind {
            b"IHDR" if data.len() >= 13 => {
                seen_ihdr = true;
                info.width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                info.height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                info.bit_depth = data[8];
                info.colour = Some(match data[9] {
                    0 => PngColour::Gray,
                    2 => PngColour::Rgb,
                    3 => PngColour::Palette,
                    4 => PngColour::GrayAlpha,
                    6 => PngColour::Rgba,
                    _ => return Ok(info),
                });
                info.interlaced = data[12] != 0;
                if matches!(
                    info.colour,
                    Some(PngColour::GrayAlpha) | Some(PngColour::Rgba)
                ) {
                    info.has_alpha = true;
                }
            }
            b"PLTE" => info.has_palette = true,
            b"tRNS" => info.has_alpha = true,
            b"iCCP" => {
                info.has_icc = true;
                info.metadata_bytes = info.metadata_bytes.saturating_add(len as u32);
            }
            b"eXIf" => {
                info.has_exif = true;
                info.metadata_bytes = info.metadata_bytes.saturating_add(len as u32);
            }
            b"IDAT" => info.idat_bytes = info.idat_bytes.saturating_add(len as u64),
            b"IEND" => break,
            b"tEXt" | b"zTXt" | b"iTXt" | b"pHYs" | b"gAMA" | b"cHRM" | b"sRGB" => {
                info.metadata_bytes = info.metadata_bytes.saturating_add(len as u32);
            }
            _ => {}
        }
        pos = data_end + 4; // skip CRC
    }
    if !seen_ihdr {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PNG without IHDR",
        ));
    }
    Ok(info)
}

#[derive(Debug, Clone, Default)]
pub struct WebpInfo {
    pub width: u32,
    pub height: u32,
    /// True for VP8L (lossless) files.
    pub lossless: bool,
    pub has_alpha: bool,
    pub has_icc: bool,
}

pub fn parse_webp(buf: &[u8]) -> io::Result<WebpInfo> {
    if buf.len() < 16 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WEBP" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a WebP (bad RIFF header)",
        ));
    }
    let mut info = WebpInfo::default();
    let mut pos = 12usize;
    while pos + 8 <= buf.len() {
        let kind = &buf[pos..pos + 4];
        let len =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        let data_start = pos + 8;
        let data_end = data_start.saturating_add(len).min(buf.len());
        let data = &buf[data_start..data_end];
        match kind {
            b"VP8 " if data.len() >= 10 => {
                // Key frame: 3-byte frame tag, then sync code, then w/h.
                if data.len() >= 10 {
                    info.width = (u16::from_le_bytes([data[6], data[7]]) & 0x3FFF) as u32;
                    info.height = (u16::from_le_bytes([data[8], data[9]]) & 0x3FFF) as u32;
                }
            }
            b"VP8L" if data.len() >= 5 => {
                info.lossless = true;
                let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                info.width = (bits & 0x3FFF) + 1;
                info.height = ((bits >> 14) & 0x3FFF) + 1;
                info.has_alpha = (bits >> 28) & 1 == 1;
            }
            b"VP8X" if data.len() >= 10 => {
                let flags = data[0];
                info.has_alpha = flags & 0x10 != 0;
                info.has_icc = flags & 0x20 != 0;
                info.width = u32::from_le_bytes([data[4], data[5], data[6], 0]) + 1;
                info.height = u32::from_le_bytes([data[7], data[8], data[9], 0]) + 1;
            }
            b"ICCP" => info.has_icc = true,
            _ => {}
        }
        pos = data_start.saturating_add(len);
        if len % 2 == 1 {
            pos += 1; // chunks are padded to an even length
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid PNG header: signature, IHDR (2x2, 8-bit RGB), IEND.
    fn tiny_png() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&PNG_SIGNATURE);
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&2u32.to_be_bytes()); // width
        v.extend_from_slice(&2u32.to_be_bytes()); // height
        v.push(8); // bit depth
        v.push(2); // colour type: RGB
        v.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC (not validated)
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"IEND");
        v.extend_from_slice(&[0, 0, 0, 0]);
        v
    }

    #[test]
    fn parses_tiny_png() {
        let info = parse(&tiny_png()).unwrap();
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(info.colour, Some(PngColour::Rgb));
        assert!(!info.has_alpha);
        assert!(info.palette_candidate());
    }

    #[test]
    fn rgba_is_not_a_palette_candidate() {
        let mut png = tiny_png();
        png[8 + 8 + 9] = 6; // colour type RGBA
        let info = parse(&png).unwrap();
        assert!(info.has_alpha);
        assert!(!info.palette_candidate());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"not a png at all").is_err());
        assert!(parse_webp(b"not a webp at all").is_err());
    }
}
