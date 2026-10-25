//! Content sniffing and the unified per-file probe.
//!
//! The format is always decided from the magic bytes, never from the file name:
//! 14.3% of the files named `.png` on one production host are actually JPEG, and
//! those files are served today because browsers sniff as well. Re-encoding a
//! JPEG inside a `.png` name is safe; renaming it is not, so we never rename.

use std::io::{self, Read};
use std::path::Path;

use crate::jpeg::{self, JpegInfo};
use crate::png::{self, PngInfo, WebpInfo};

/// How much of a file we read to parse its headers. Large enough for a JPEG with
/// a 64 KB ICC profile plus an XMP block; small enough that a 2.1 million file
/// scan reads a few gigabytes at most.
pub const HEAD_BYTES: usize = 192 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
    Webp,
    Gif,
    Avif,
    Heic,
    Jxl,
    Tiff,
    Bmp,
    Other,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Jpeg => "jpeg",
            Format::Png => "png",
            Format::Webp => "webp",
            Format::Gif => "gif",
            Format::Avif => "avif",
            Format::Heic => "heic",
            Format::Jxl => "jxl",
            Format::Tiff => "tiff",
            Format::Bmp => "bmp",
            Format::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Content {
    pub format: Option<Format>,
    pub jpeg: Option<JpegInfo>,
    pub png: Option<PngInfo>,
    pub webp: Option<WebpInfo>,
}

impl Content {
    pub fn format(&self) -> Format {
        self.format.unwrap_or(Format::Other)
    }

    pub fn width(&self) -> u32 {
        if let Some(j) = &self.jpeg {
            j.width
        } else if let Some(p) = &self.png {
            p.width
        } else if let Some(w) = &self.webp {
            w.width
        } else {
            0
        }
    }

    pub fn height(&self) -> u32 {
        if let Some(j) = &self.jpeg {
            j.height
        } else if let Some(p) = &self.png {
            p.height
        } else if let Some(w) = &self.webp {
            w.height
        } else {
            0
        }
    }

    pub fn longest_side(&self) -> u32 {
        self.width().max(self.height())
    }
}

pub fn sniff(head: &[u8]) -> Format {
    if head.len() >= 3 && head[0] == 0xFF && head[1] == 0xD8 && head[2] == 0xFF {
        return Format::Jpeg;
    }
    if head.len() >= 8 && head[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Format::Png;
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Format::Webp;
    }
    if head.len() >= 6 && (&head[0..6] == b"GIF87a" || &head[0..6] == b"GIF89a") {
        return Format::Gif;
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        match brand {
            b"avif" | b"avis" => return Format::Avif,
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                return Format::Heic
            }
            _ => {}
        }
    }
    // JPEG XL: bare codestream starts with FF 0A; the container starts with a
    // 12-byte signature box: 00 00 00 0C 'JXL ' 0D 0A 87 0A.
    if head.len() >= 2 && head[0] == 0xFF && head[1] == 0x0A {
        return Format::Jxl;
    }
    if head.len() >= 12 && head[0..2] == [0x00, 0x00] && &head[4..8] == b"JXL " {
        return Format::Jxl;
    }
    if head.len() >= 4 && ((&head[0..4] == b"II*\0") || (&head[0..4] == b"MM\0*")) {
        return Format::Tiff;
    }
    if head.len() >= 2 && &head[0..2] == b"BM" {
        return Format::Bmp;
    }
    Format::Other
}

/// Read the head of a file and parse whatever headers it has.
pub fn probe(path: &Path) -> io::Result<Content> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; HEAD_BYTES];
    let mut filled = 0usize;
    loop {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => {
                filled += n;
                if filled == buf.len() {
                    break;
                }
            }
        }
    }
    buf.truncate(filled);

    let format = sniff(&buf);
    let mut content = Content {
        format: Some(format),
        ..Default::default()
    };
    match format {
        Format::Jpeg => {
            if let Ok(info) = jpeg::parse(&buf) {
                content.jpeg = Some(info);
            }
        }
        Format::Png => {
            if let Ok(info) = png::parse(&buf) {
                content.png = Some(info);
            }
        }
        Format::Webp => {
            if let Ok(info) = png::parse_webp(&buf) {
                content.webp = Some(info);
            }
        }
        _ => {}
    }
    Ok(content)
}

/// Probe a byte slice instead of a file. Used by tests and by the remote mode,
/// which receives heads over the wire instead of whole files.
pub fn probe_bytes(buf: &[u8]) -> Content {
    let format = sniff(buf);
    let mut content = Content {
        format: Some(format),
        ..Default::default()
    };
    match format {
        Format::Jpeg => content.jpeg = jpeg::parse(buf).ok(),
        Format::Png => content.png = png::parse(buf).ok(),
        Format::Webp => content.webp = png::parse_webp(buf).ok(),
        _ => {}
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_known_magics() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Format::Jpeg);
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Format::Png
        );
        assert_eq!(sniff(b"GIF89a......"), Format::Gif);
        assert_eq!(sniff(b"\x00\x00\x00\x0cJXL \x0d\x0a\x87\x0a"), Format::Jxl);
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00]), Format::Jxl);
        assert_eq!(sniff(b"II*\x00rest"), Format::Tiff);
        assert_eq!(sniff(b"BM1234"), Format::Bmp);
        assert_eq!(sniff(b"<?xml version"), Format::Other);
        assert_eq!(sniff(b""), Format::Other);
    }

    #[test]
    fn sniffs_webp_and_avif() {
        let mut webp = Vec::from(*b"RIFF\0\0\0\0");
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff(&webp), Format::Webp);

        let mut avif = Vec::from(*b"\0\0\0\x20");
        avif.extend_from_slice(b"ftypavif\0\0\0\0");
        assert_eq!(sniff(&avif), Format::Avif);
    }
}
